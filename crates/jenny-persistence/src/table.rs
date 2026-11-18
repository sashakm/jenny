//! A [`Store`] over a table of your own: one column per field.

use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use jenny_core::domain::{Aggregate, Criteria, DomainError, DomainResult, ValueKind};
use jenny_core::port::store::{Loaded, Store, Version, encode, expect_version};
use serde_json::{Map, Value};
use sqlx::{AssertSqlSafe, PgPool};

use crate::criteria::{Columns, SqlCriteria};
use crate::documents::is_identifier;
use crate::error::RepositoryError;

/// Aggregates of one kind in a table whose columns are their fields, for data that
/// other systems — reports, a BI tool, a hand-written join — query relationally.
///
/// The schema is yours, written as a migration. The store needs two things of it: a
/// column for every top-level field the aggregate serialises to, named the same, and a
/// `version bigint` column. Nested values go into `jsonb` columns, lists into arrays or
/// `jsonb`, and columns the aggregate does not have — `created_at DEFAULT now()` — are
/// left to the database:
///
/// ```sql
/// CREATE TABLE notes (
///     id         uuid        PRIMARY KEY,
///     version    bigint      NOT NULL,
///     title      text        NOT NULL,
///     body       text        NOT NULL,
///     archived   boolean     NOT NULL,
///     created_at timestamptz NOT NULL DEFAULT now()
/// );
/// ```
///
/// No mapping code is needed: rows are read with `to_jsonb` and written with
/// `jsonb_populate_record`, so Postgres converts between the aggregate's serde form and
/// the column types. The runtime reads the table's columns from the catalogue before it
/// serves, and refuses to start when `id` or `version` is missing. An aggregate field
/// with no column is refused on the first write, rather than silently not stored.
///
/// The aggregate must not `#[serde(deny_unknown_fields)]`: a row carries the columns it
/// does not have.
pub struct PgTable<A> {
    pool: PgPool,
    shape: Arc<TableShape>,
    columns: Columns,
    kind: PhantomData<fn() -> A>,
}

/// A table's name and, once read from the catalogue, its columns.
pub(crate) struct TableShape {
    name: String,
    columns: OnceLock<Vec<String>>,
}

impl TableShape {
    pub(crate) fn new(name: &str) -> Arc<Self> {
        assert!(
            is_identifier(name),
            "`{name}` is not usable as a table name: use lower-case letters, digits and \
             underscores, starting with a letter"
        );
        Arc::new(Self {
            name: name.to_string(),
            columns: OnceLock::new(),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The table's columns, read from the catalogue the first time they are needed.
    pub(crate) async fn columns(&self, pool: &PgPool) -> DomainResult<&[String]> {
        if let Some(columns) = self.columns.get() {
            return Ok(columns);
        }
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
             ORDER BY ordinal_position",
        )
        .bind(&self.name)
        .fetch_all(pool)
        .await
        .map_err(database)?;

        if columns.is_empty() {
            return Err(DomainError::internal(format!(
                "table `{}` does not exist; create it with a migration",
                self.name
            )));
        }
        for required in ["id", "version"] {
            if !columns.iter().any(|column| column == required) {
                return Err(DomainError::internal(format!(
                    "table `{}` has no `{required}` column, which a table store needs",
                    self.name
                )));
            }
        }
        // Another task may have got here first; either answer is the same.
        Ok(self.columns.get_or_init(|| columns))
    }
}

impl<A: Aggregate> PgTable<A> {
    /// Usually reached through [`Postgres::table`](crate::Postgres::table), which also
    /// checks the table before the service serves.
    ///
    /// Panics when `table` is not a plain lower-case SQL identifier.
    pub fn new(pool: PgPool, table: &str) -> Self {
        Self::with_shape(pool, TableShape::new(table))
    }

    pub(crate) fn with_shape(pool: PgPool, shape: Arc<TableShape>) -> Self {
        Self {
            pool,
            shape,
            columns: Columns::new("t.id", []),
            kind: PhantomData,
        }
    }

    /// The attribute paths a listing may sort and filter on, and what each holds.
    ///
    /// A top-level path is its column; a dotted one reaches into a `jsonb` column
    /// (`author.name` is `(t."author" #>> '{name}')`). Panics when a path is not dotted
    /// field names, since it is written into the statement.
    pub fn queryable<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = (P, ValueKind)>,
        P: AsRef<str>,
    {
        let mut columns = Columns::new("t.id", []);
        for (path, kind) in paths {
            let path = path.as_ref();
            columns = columns.insert(path, &expression(path, kind), kind);
        }
        self.columns = columns;
        self
    }

    fn table(&self) -> String {
        quote(self.shape.name())
    }

    /// `t.id` equal to the id written as text, converted to whatever type the column
    /// has the same way a written row is: through the table's own record type.
    fn id_matches(&self, placeholder: &str) -> String {
        format!(
            "t.id = (jsonb_populate_record(NULL::{}, jsonb_build_object('id', {placeholder}::text))).id",
            self.table()
        )
    }

    /// The aggregate's fields, each checked to have a column.
    async fn fields(&self, aggregate: &A) -> DomainResult<(Map<String, Value>, Vec<String>)> {
        let Value::Object(document) = encode(aggregate)? else {
            return Err(DomainError::internal(format!(
                "a {} does not serialise to an object, so it has no fields for columns",
                A::KIND
            )));
        };
        let columns = self.shape.columns(&self.pool).await?;
        let mut fields = Vec::new();
        for field in document.keys() {
            if field == "version" {
                return Err(DomainError::internal(format!(
                    "a {} has a field called `version`, which a table store keeps for itself",
                    A::KIND
                )));
            }
            if !columns.iter().any(|column| column == field) {
                return Err(DomainError::internal(format!(
                    "a {} has a field `{field}` that table `{}` has no column for; \
                     it would not be stored",
                    A::KIND,
                    self.shape.name()
                )));
            }
            fields.push(field.clone());
        }
        Ok((document, fields))
    }

    /// A row as read: the aggregate, and the version beside it.
    fn decode(mut row: Value) -> DomainResult<Loaded<A>> {
        let version = row
            .as_object_mut()
            .and_then(|columns| columns.remove("version"))
            .and_then(|version| version.as_i64())
            .ok_or_else(|| DomainError::internal("a stored row has no usable `version`"))?;
        let aggregate = Loaded::<A>::decode(row, stored_version(version))?.into_inner();
        // Compare later changes against the aggregate's own encoding, not the row: the
        // row also holds columns the aggregate does not, which are not changes.
        Loaded::written(aggregate, stored_version(version))
    }

    async fn current_version(&self, key: &str) -> DomainResult<Option<Version>> {
        let statement = format!(
            "SELECT t.version FROM {} t WHERE {}",
            self.table(),
            self.id_matches("$1")
        );
        let version: Option<i64> = sqlx::query_scalar(AssertSqlSafe(statement))
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        Ok(version.map(stored_version))
    }
}

#[async_trait]
impl<A: Aggregate> Store<A> for PgTable<A> {
    async fn find(&self, id: &A::Id) -> DomainResult<Option<Loaded<A>>> {
        let statement = format!(
            "SELECT to_jsonb(t) FROM {} t WHERE {}",
            self.table(),
            self.id_matches("$1")
        );
        let row: Option<Value> = sqlx::query_scalar(AssertSqlSafe(statement))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        row.map(Self::decode).transpose()
    }

    async fn insert(&self, aggregate: A) -> DomainResult<Loaded<A>> {
        let (document, fields) = self.fields(&aggregate).await?;
        let statement = format!(
            "INSERT INTO {table} ({columns}, version) \
             SELECT {from_record}, 1 FROM jsonb_populate_record(NULL::{table}, $1) r \
             ON CONFLICT DO NOTHING",
            table = self.table(),
            columns = column_list(&fields, ""),
            from_record = column_list(&fields, "r."),
        );
        let inserted = sqlx::query(AssertSqlSafe(statement))
            .bind(Value::Object(document))
            .execute(&self.pool)
            .await
            .map_err(database)?;
        if inserted.rows_affected() == 0 {
            return Err(DomainError::conflict(format!(
                "{} {} already exists",
                A::KIND,
                aggregate.id()
            )));
        }
        Loaded::written(aggregate, Version::FIRST)
    }

    async fn save(&self, aggregate: Loaded<A>) -> DomainResult<Loaded<A>> {
        let key = aggregate.id().to_string();
        let read_at = aggregate.version();
        let (document, fields) = self.fields(&aggregate).await?;
        let changed: Vec<String> = fields.into_iter().filter(|f| f != "id").collect();
        let assignments = if changed.is_empty() {
            "version = t.version + 1".to_string()
        } else {
            format!(
                "({columns}) = (SELECT {from_record} FROM jsonb_populate_record(NULL::{table}, $3) r), \
                 version = t.version + 1",
                table = self.table(),
                columns = column_list(&changed, ""),
                from_record = column_list(&changed, "r."),
            )
        };
        let statement = format!(
            "UPDATE {} t SET {assignments} WHERE {} AND t.version = $2 RETURNING t.version",
            self.table(),
            self.id_matches("$1"),
        );
        let saved: Option<i64> = sqlx::query_scalar(AssertSqlSafe(statement))
            .bind(&key)
            .bind(database_version(read_at))
            .bind(Value::Object(document))
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        match saved {
            Some(version) => Loaded::written(aggregate.into_inner(), stored_version(version)),
            None => match self.current_version(&key).await? {
                None => Err(DomainError::not_found()),
                Some(current) => Err(DomainError::conflict(format!(
                    "{} {key} was changed by someone else since it was read \
                     (read at version {read_at}, now at {current})",
                    A::KIND
                ))),
            },
        }
    }

    async fn remove(&self, id: &A::Id, expected: Option<Version>) -> DomainResult<A> {
        let key = id.to_string();
        let statement = format!(
            "DELETE FROM {} t WHERE {} AND ($2::bigint IS NULL OR t.version = $2) \
             RETURNING to_jsonb(t)",
            self.table(),
            self.id_matches("$1")
        );
        let removed: Option<Value> = sqlx::query_scalar(AssertSqlSafe(statement))
            .bind(&key)
            .bind(expected.map(database_version))
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        match removed {
            Some(row) => Ok(Self::decode(row)?.into_inner()),
            None => match self.current_version(&key).await? {
                None => Err(DomainError::not_found()),
                Some(current) => {
                    expect_version::<A>(id, expected, current)?;
                    Err(DomainError::conflict(format!(
                        "{} {key} changed while it was being removed",
                        A::KIND
                    )))
                }
            },
        }
    }

    async fn list(&self, criteria: &Criteria) -> DomainResult<Vec<A>> {
        // Identifiers come from `queryable` and the table name; every value the client
        // sent is one of `sql.bindings()`.
        let sql = SqlCriteria::build(criteria, &self.columns)?;
        let statement = format!("SELECT to_jsonb(t) FROM {} t{}", self.table(), sql.tail());
        let mut query = sqlx::query_scalar::<_, Value>(AssertSqlSafe(statement));
        for binding in sql.bindings() {
            query = query.bind(binding);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(database)?;
        rows.into_iter()
            .map(|row| Ok(Self::decode(row)?.into_inner()))
            .collect()
    }
}

impl<A> fmt::Debug for PgTable<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgTable")
            .field("table", &self.shape.name())
            .field("queryable", &self.columns.paths())
            .finish_non_exhaustive()
    }
}

/// `"title", "body"`, or with a prefix, `r."title", r."body"`.
fn column_list(columns: &[String], prefix: &str) -> String {
    columns
        .iter()
        .map(|column| format!("{prefix}{}", quote(column)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A double-quoted identifier. Names come from the catalogue or from code, but quoting
/// them properly costs nothing and makes the statement safe whatever they hold.
fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// `title` → `t."title"`; `author.name` as text → `(t."author" #>> '{name}')`.
fn expression(path: &str, kind: ValueKind) -> String {
    let mut segments = path.split('.');
    let column = segments.next().unwrap_or_default();
    let rest: Vec<&str> = segments.collect();
    assert!(
        std::iter::once(column)
            .chain(rest.iter().copied())
            .all(crate::documents::is_field),
        "`{path}` is not a dotted path of field names"
    );
    if rest.is_empty() {
        // The column has its own type; the bound value is cast to match it.
        return format!("t.{}", quote(column));
    }
    crate::documents::cast_text(
        &format!("(t.{} #>> '{{{}}}')", quote(column), rest.join(",")),
        kind,
    )
}

fn stored_version(version: i64) -> Version {
    Version::new(version.max(1) as u64)
}

fn database_version(version: Version) -> i64 {
    i64::try_from(version.get()).unwrap_or(i64::MAX)
}

fn database(error: sqlx::Error) -> DomainError {
    RepositoryError::from(error).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_top_level_path_is_its_column_and_a_dotted_one_reaches_into_jsonb() {
        assert_eq!(expression("title", ValueKind::Text), "t.\"title\"");
        assert_eq!(expression("priority", ValueKind::Integer), "t.\"priority\"");
        assert_eq!(
            expression("author.age", ValueKind::Integer),
            "((t.\"author\" #>> '{age}')::bigint)"
        );
    }

    #[test]
    fn identifiers_are_quoted_with_embedded_quotes_doubled() {
        assert_eq!(quote("title"), "\"title\"");
        assert_eq!(quote("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(
            column_list(&["a".into(), "b".into()], "r."),
            "r.\"a\", r.\"b\""
        );
    }
}
