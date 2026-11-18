//! Apache AGE: openCypher graphs inside the same Postgres, through the same pool.
//!
//! A graph's value is in traversals, which `Store<A>` cannot express, so there is no
//! graph port here: the port is your domain's (`trait Connections { async fn
//! friends_of(…) }`), and its adapter uses a [`Graph`] the way a relational adapter uses
//! [`SqlCriteria`](crate::SqlCriteria) (ADR-013, ADR-019).
//!
//! ```no_run
//! # use jenny_persistence::Postgres;
//! # async fn example(postgres: Postgres) -> jenny_core::domain::DomainResult<()> {
//! let social = postgres.graph("social").identifiers(["KNOWS", "FOLLOWS"]);
//!
//! let names: Vec<String> = social
//!     .cypher("MATCH (:Person {id: $id})-[:{{relation}}]->(f:Person) RETURN f.name")
//!     .bind("id", 7)
//!     .identifier("relation", "KNOWS")
//!     .fetch_all()
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! ### What keeps it safe
//!
//! - **Queries are `&'static str`.** Values go in as `$parameters`, bound as one
//!   `agtype` map, never spliced into the text.
//! - **Labels and relationship types cannot be parameters** in Cypher, so a query names
//!   them through a `{{placeholder}}`, and [`identifier`](Cypher::identifier) fills it
//!   only with a name from the graph's allowlist — the same answer filter paths get
//!   (ADR-010). A name that is not on it is a 400, since it most likely came from a
//!   client.
//!
//! ### What a query returns
//!
//! One value per row, deserialised with serde: `RETURN f.name` into a `String`,
//! `RETURN {name: f.name, since: r.since}` into a struct of yours. A vertex arrives as
//! `{"id", "label", "properties"}`, an edge with `start_id` and `end_id` too, a path as
//! an array of those.
//!
//! ### What the database needs
//!
//! The AGE extension, installed. [`Postgres::prepare`](crate::Postgres::prepare)
//! creates it and every graph handed out when `create_tables` is on; otherwise it
//! checks that they exist, and [`Graph::schema`] is the SQL for a migration. A role
//! that is not a superuser may not `LOAD` the library, so for one, preload it
//! (`shared_preload_libraries` or `session_preload_libraries`); queries load it
//! themselves only when it is not preloaded.

use std::collections::BTreeSet;
use std::sync::Arc;

use jenny_core::domain::{DomainError, DomainResult};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use sqlx::postgres::{PgArgumentBuffer, PgTypeInfo, PgValueFormat, PgValueRef};
use sqlx::{AssertSqlSafe, PgConnection, PgPool, Postgres, Transaction};
use tokio::sync::OnceCell;

/// The dollar-quote tag around a query. A query containing it is refused.
const QUOTE: &str = "$jenny_cypher$";

/// `ag_catalog` at the front of the search path, for the rest of the transaction:
/// AGE's functions and rewritten queries look up its operators unqualified.
/// Prepended, so the caller's own schemas still resolve.
const SEARCH_PATH: &str = "SELECT set_config('search_path', \
                               'ag_catalog, ' || current_setting('search_path'), true) \
                           WHERE NOT 'ag_catalog' = ANY (current_schemas(false))";

/// An AGE graph: the name it has in `ag_catalog.ag_graph`, the pool it is queried
/// through, and the labels and relationship types queries may name by placeholder.
///
/// Handed out by [`Postgres::graph`](crate::Postgres::graph). Cheap to clone.
#[derive(Clone)]
pub struct Graph {
    pool: PgPool,
    name: Arc<str>,
    identifiers: Arc<BTreeSet<String>>,
    /// Whether each transaction must `LOAD 'age'` first: only when the server does not
    /// preload it, since a role that is not a superuser may not `LOAD`.
    load: Arc<OnceCell<bool>>,
}

impl Graph {
    /// Panics when `name` is not a plain lower-case identifier: it is a constant in
    /// your code.
    pub(crate) fn new(pool: PgPool, name: &str) -> Self {
        assert!(
            is_graph_name(name),
            "`{name}` is not a graph name: use lower-case letters, digits and `_`, \
             starting with a letter"
        );
        Self {
            pool,
            name: name.into(),
            identifiers: Arc::default(),
            load: Arc::default(),
        }
    }

    /// The labels and relationship types a query may name through a placeholder.
    ///
    /// Panics on a name that is not a plain identifier.
    pub fn identifiers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut allowed = (*self.identifiers).clone();
        for name in names {
            let name = name.into();
            assert!(
                is_identifier(&name),
                "`{name}` is not a label or relationship type"
            );
            allowed.insert(name);
        }
        self.identifiers = Arc::new(allowed);
        self
    }

    /// The graph's name in `ag_catalog.ag_graph`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A query against this graph. See the [module docs](self) for what it may contain
    /// and return.
    pub fn cypher(&self, query: &'static str) -> Cypher<'_> {
        Cypher {
            graph: self,
            query,
            parameters: Map::new(),
            identifiers: Vec::new(),
            error: None,
        }
    }

    /// The SQL that creates the extension and this graph, for a migration of your own
    /// when `create_tables` is off. It must run in a transaction, as migrations do,
    /// because the search path it sets is local to one. Not idempotent:
    /// `create_graph` fails on a graph that exists, which a migration that runs once
    /// never meets.
    pub fn schema(&self) -> String {
        format!(
            "CREATE EXTENSION IF NOT EXISTS age;\n\
             {SEARCH_PATH};\n\
             SELECT ag_catalog.create_graph('{}');",
            self.name
        )
    }

    /// Create the extension and the graph if they do not exist, inside the caller's
    /// transaction, which holds the schema lock.
    pub(crate) async fn create(&self, connection: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query("CREATE EXTENSION IF NOT EXISTS age")
            .execute(&mut *connection)
            .await?;
        self.enter(connection).await?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ag_catalog.ag_graph WHERE name = $1)")
                .bind(&*self.name)
                .fetch_one(&mut *connection)
                .await?;
        if !exists {
            sqlx::query("SELECT ag_catalog.create_graph($1)")
                .bind(&*self.name)
                .execute(&mut *connection)
                .await?;
        }
        Ok(())
    }

    /// Whether the extension is installed and the graph exists.
    pub(crate) async fn exists(&self) -> sqlx::Result<bool> {
        // `to_regclass` first: without the extension, `ag_catalog.ag_graph` does not
        // resolve, and naming it would be an error rather than `false`.
        let catalogue: bool =
            sqlx::query_scalar("SELECT to_regclass('ag_catalog.ag_graph') IS NOT NULL")
                .fetch_one(&self.pool)
                .await?;
        if !catalogue {
            return Ok(false);
        }
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ag_catalog.ag_graph WHERE name = $1)")
            .bind(&*self.name)
            .fetch_one(&self.pool)
            .await
    }

    /// What every statement needs, inside the caller's transaction: AGE loaded, and
    /// `ag_catalog` on the search path. The search path is local to the transaction, so
    /// a pooled connection goes back with its own; a `LOAD` lasts for the connection,
    /// which only spares the next transaction one.
    async fn enter(&self, connection: &mut PgConnection) -> sqlx::Result<()> {
        let load = *self
            .load
            .get_or_try_init(|| async {
                let preloaded: bool = sqlx::query_scalar(
                    "SELECT 'age' = ANY (string_to_array(replace(\
                         current_setting('shared_preload_libraries') || ',' || \
                         current_setting('session_preload_libraries'), ' ', ''), ','))",
                )
                .fetch_one(&mut *connection)
                .await?;
                Ok::<_, sqlx::Error>(!preloaded)
            })
            .await?;
        if load {
            sqlx::raw_sql(AssertSqlSafe(format!("LOAD 'age'; {SEARCH_PATH}")))
                .execute(&mut *connection)
                .await?;
        } else {
            sqlx::raw_sql(SEARCH_PATH).execute(&mut *connection).await?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("name", &self.name)
            .field("identifiers", &self.identifiers)
            .finish_non_exhaustive()
    }
}

/// One Cypher statement, being given its parameters. Nothing runs until a `fetch_*` or
/// [`execute`](Self::execute).
#[must_use = "a query does nothing until it is fetched or executed"]
pub struct Cypher<'g> {
    graph: &'g Graph,
    query: &'static str,
    parameters: Map<String, Value>,
    identifiers: Vec<(String, String)>,
    /// The first thing wrong with the parameters, reported when the query runs.
    error: Option<DomainError>,
}

impl Cypher<'_> {
    /// Bind `$name` to `value`, as an `agtype` value.
    pub fn bind(mut self, name: &str, value: impl Serialize) -> Self {
        match serde_json::to_value(value) {
            Ok(value) => {
                self.parameters.insert(name.to_string(), value);
            }
            Err(e) => {
                self.error.get_or_insert_with(|| {
                    DomainError::internal(format!("parameter `${name}` does not serialise: {e}"))
                });
            }
        }
        self
    }

    /// Fill the placeholder `{{placeholder}}` with a label or relationship type, which
    /// must be one the graph [allows](Graph::identifiers).
    pub fn identifier(mut self, placeholder: &str, name: &str) -> Self {
        if self.graph.identifiers.contains(name) {
            self.identifiers
                .push((format!("{{{{{placeholder}}}}}"), name.to_string()));
        } else {
            self.error.get_or_insert_with(|| {
                DomainError::invalid(format!("`{name}` is not a label or relationship here"))
            });
        }
        self
    }

    /// Every row, each deserialised into a `T`.
    pub async fn fetch_all<T: DeserializeOwned>(self) -> DomainResult<Vec<T>> {
        let mut transaction = self.graph.pool.begin().await.map_err(failure)?;
        let rows = self.fetch_all_in(&mut transaction).await?;
        transaction.commit().await.map_err(failure)?;
        Ok(rows)
    }

    /// The only row, or `None`; more than one is an internal error.
    pub async fn fetch_optional<T: DeserializeOwned>(self) -> DomainResult<Option<T>> {
        let mut rows = self.fetch_all::<T>().await?;
        match rows.len() {
            0 | 1 => Ok(rows.pop()),
            n => Err(DomainError::internal(format!(
                "a query expected to return at most one row returned {n}"
            ))),
        }
    }

    /// Run it for its effect: a `CREATE`, `MERGE`, `SET` or `DELETE`.
    pub async fn execute(self) -> DomainResult<()> {
        self.fetch_all::<Value>().await.map(drop)
    }

    /// [`fetch_all`](Self::fetch_all) inside a transaction of yours, so graph writes
    /// commit together with each other and with ordinary tables.
    pub async fn fetch_all_in<T: DeserializeOwned>(
        mut self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> DomainResult<Vec<T>> {
        let statement = self.statement()?;
        self.graph.enter(transaction).await.map_err(failure)?;
        let rows: Vec<Agtype> = sqlx::query_scalar(AssertSqlSafe(statement))
            .bind(Agtype(Value::Object(self.parameters)))
            .fetch_all(&mut **transaction)
            .await
            .map_err(failure)?;
        rows.into_iter()
            .map(|Agtype(value)| {
                serde_json::from_value(value)
                    .map_err(|e| DomainError::internal(format!("a graph row does not decode: {e}")))
            })
            .collect()
    }

    /// The SQL around the query, with the placeholders filled.
    fn statement(&mut self) -> DomainResult<String> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        let mut query = self.query.to_string();
        for (placeholder, name) in &self.identifiers {
            query = query.replace(placeholder.as_str(), name);
        }
        if query.contains("{{") {
            return Err(DomainError::internal(format!(
                "query has a placeholder no identifier was given for: {query}"
            )));
        }
        if query.contains(QUOTE) {
            return Err(DomainError::internal(
                "a Cypher query may not contain `$jenny_cypher$`",
            ));
        }
        // The graph name was validated as an identifier, so it needs no escaping.
        Ok(format!(
            "SELECT result FROM ag_catalog.cypher('{}', {QUOTE}{query}{QUOTE}, $1) \
             AS (result ag_catalog.agtype)",
            self.graph.name
        ))
    }
}

/// A database failure, as the caller can do nothing about it.
fn failure(error: sqlx::Error) -> DomainError {
    DomainError::internal_from(crate::RepositoryError::from(error))
}

/// An `agtype` value, as JSON.
struct Agtype(Value);

impl sqlx::Type<Postgres> for Agtype {
    fn type_info() -> PgTypeInfo {
        // Qualified, so it resolves whatever the search path.
        PgTypeInfo::with_name("ag_catalog.agtype")
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        // A result column is named as the search path finds it, which puts
        // `ag_catalog` first.
        matches!(sqlx::TypeInfo::name(ty), "agtype" | "ag_catalog.agtype")
    }
}

impl sqlx::Encode<'_, Postgres> for Agtype {
    fn encode_by_ref(
        &self,
        buf: &mut PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        // The binary format is jsonb's: a version byte, then the text.
        buf.push(1);
        serde_json::to_writer(&mut **buf, &self.0)?;
        Ok(sqlx::encode::IsNull::No)
    }
}

impl sqlx::Decode<'_, Postgres> for Agtype {
    fn decode(value: PgValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        let format = value.format();
        let mut bytes = value.as_bytes()?;
        if format == PgValueFormat::Binary {
            match bytes.split_first() {
                Some((1, rest)) => bytes = rest,
                _ => return Err("unsupported agtype binary format".into()),
            }
        }
        Ok(Agtype(to_json(std::str::from_utf8(bytes)?)?))
    }
}

/// agtype's text form is JSON with type annotations after values — `{…}::vertex`,
/// `[…]::path`, `1.5::numeric` — which are dropped; outside strings, nothing else
/// differs. `NaN` and the infinities have no JSON form and are refused.
fn to_json(text: &str) -> Result<Value, String> {
    let mut json = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            json.push(c);
            match c {
                '\\' => json.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                json.push(c);
            }
            ':' if chars.peek() == Some(&':') => {
                chars.next();
                while chars.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    chars.next();
                }
            }
            _ => json.push(c),
        }
    }
    serde_json::from_str(&json)
        .map_err(|e| format!("`{text}` is not a value jenny can decode: {e}"))
}

fn is_graph_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 63
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn graph() -> Graph {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        Graph::new(pool, "social").identifiers(["KNOWS"])
    }

    #[test]
    fn annotations_are_dropped_and_strings_left_alone() {
        assert_eq!(
            to_json(r#"{"id": 1, "label": "Person", "properties": {"name": "a::b"}}::vertex"#)
                .unwrap(),
            json!({ "id": 1, "label": "Person", "properties": { "name": "a::b" } })
        );
        assert_eq!(
            to_json(r#"[{"id": 1}::vertex, {"id": 2}::edge, {"id": 3}::vertex]::path"#).unwrap(),
            json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }])
        );
        assert_eq!(to_json("1.5::numeric").unwrap(), json!(1.5));
        assert_eq!(
            to_json(r#""say \"hi\"::x""#).unwrap(),
            json!("say \"hi\"::x")
        );
        assert!(to_json("NaN").is_err());
    }

    #[test]
    fn names_that_reach_the_sql_are_plain_identifiers() {
        assert!(is_graph_name("social_2"));
        for bad in ["", "Social", "2social", "so-cial", "x'); DROP"] {
            assert!(!is_graph_name(bad), "{bad}");
        }
        assert!(is_identifier("KNOWS"));
        for bad in ["", "1A", "KNOWS]->()", "a b"] {
            assert!(!is_identifier(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn only_allowed_names_fill_a_placeholder() {
        let graph = graph();
        let statement = graph
            .cypher("MATCH (a)-[:{{relation}}]->(b) RETURN b")
            .identifier("relation", "KNOWS")
            .statement()
            .unwrap();
        assert!(statement.contains("-[:KNOWS]->"), "{statement}");
        assert!(statement.starts_with("SELECT result FROM ag_catalog.cypher('social', "));

        let refused = graph
            .cypher("MATCH (a)-[:{{relation}}]->(b) RETURN b")
            .identifier("relation", "KNOWS]->(x) DETACH DELETE x //")
            .statement()
            .unwrap_err();
        assert!(
            matches!(refused, DomainError::Invalid { .. }),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn a_query_that_could_leave_its_quotes_is_refused() {
        let graph = graph();
        let unfilled = graph.cypher("MATCH (a:{{label}}) RETURN a").statement();
        assert!(unfilled.is_err());
        let escaping = graph
            .cypher("RETURN 1 $jenny_cypher$, NULL) AS (r agtype); DROP TABLE x; --")
            .statement();
        assert!(escaping.is_err());
    }

    #[tokio::test]
    async fn the_migration_names_the_graph() {
        let schema = graph().schema();
        assert!(
            schema.starts_with("CREATE EXTENSION IF NOT EXISTS age;\n"),
            "{schema}"
        );
        assert!(
            schema.ends_with("SELECT ag_catalog.create_graph('social');"),
            "{schema}"
        );
    }

    #[tokio::test]
    #[should_panic(expected = "not a graph name")]
    async fn a_graph_name_that_is_not_an_identifier_panics() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        let _ = Graph::new(pool, "Social Network");
    }
}
