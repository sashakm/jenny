//! Turning [`Criteria`] into SQL, safely.
//!
//! A [`Filter`](jenny_core::domain::Filter) names its target with a client-supplied string. Interpolating that into
//! SQL would be an injection vector, and binding it as a parameter does not help —
//! parameters cannot stand in for identifiers. The only sound answer is a fixed
//! translation table from attribute path to column, decided by the code that knows the
//! schema.
//!
//! So [`Columns`] is a **required argument**, not an option. There is deliberately no
//! function here that turns criteria into SQL without one.
//!
//! Three separate things are handled:
//!
//! 1. **Identifiers** come from [`Columns`] — static strings you wrote — and are the
//!    only part interpolated into the statement.
//! 2. **Values** are never interpolated. They come back as ordered bindings.
//! 3. **Types** are checked while building, so `age:gt:banana` is a 400 at the edge
//!    rather than a 500 from the database.
//!
//! ```
//! use jenny_core::domain::Criteria;
//! use jenny_persistence::criteria::{ColumnKind, Columns, SqlCriteria};
//!
//! let columns = Columns::new([
//!     ("title", "title", ColumnKind::Text),
//!     ("created_at", "created_at", ColumnKind::Timestamp),
//!     ("author.name", "a.name", ColumnKind::Text),
//! ]);
//!
//! let criteria = Criteria::new()
//!     .with_filter("title:contains:milk".parse()?)
//!     .with_sort("-created_at".parse()?);
//!
//! let sql = SqlCriteria::build(&criteria, &columns)?;
//! assert_eq!(sql.where_clause(), " WHERE title ILIKE $1");
//! assert_eq!(sql.order_by(), " ORDER BY created_at DESC");
//! assert_eq!(sql.bindings(), ["%milk%"]);
//! # Ok::<(), jenny_core::domain::DomainError>(())
//! ```

use std::collections::BTreeMap;

use jenny_core::domain::{Check, Criteria, DomainError, DomainResult, Ordering};

/// What a column holds, so values can be checked and cast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    Text,
    Integer,
    Float,
    Bool,
    Uuid,
    Timestamp,
}

impl ColumnKind {
    /// The Postgres type a bound parameter is cast to. Bindings go over the wire as
    /// text, so a comparison against a non-text column needs an explicit cast.
    fn cast(&self) -> Option<&'static str> {
        match self {
            ColumnKind::Text => None,
            ColumnKind::Integer => Some("::bigint"),
            ColumnKind::Float => Some("::double precision"),
            ColumnKind::Bool => Some("::boolean"),
            ColumnKind::Uuid => Some("::uuid"),
            ColumnKind::Timestamp => Some("::timestamptz"),
        }
    }

    /// Reject a value the database would only reject later, and less helpfully.
    fn check_value(&self, path: &str, value: &str) -> DomainResult<()> {
        let ok = match self {
            ColumnKind::Text => true,
            ColumnKind::Integer => value.parse::<i64>().is_ok(),
            ColumnKind::Float => value.parse::<f64>().is_ok(),
            ColumnKind::Bool => matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "false" | "t" | "f" | "1" | "0"
            ),
            ColumnKind::Uuid => uuid::Uuid::parse_str(value).is_ok(),
            // Left to the database: date formats are many and Postgres is better at
            // them than a hand-rolled check would be.
            ColumnKind::Timestamp => true,
        };
        if ok {
            Ok(())
        } else {
            Err(DomainError::invalid(format!(
                "`{value}` is not a valid value for `{path}`"
            )))
        }
    }

    fn supports(&self, check: Check) -> bool {
        match check {
            // Substring checks only mean something for text.
            Check::Begins | Check::Ends | Check::Contains => *self == ColumnKind::Text,
            Check::Eq => true,
            Check::Gt | Check::St => *self != ColumnKind::Bool,
        }
    }
}

/// The attribute paths a repository is willing to be queried on, and the columns they
/// map to.
///
/// Doubles as the allowlist and the translation table: a path that is not in here
/// cannot be queried at all, and one that is can only produce the column you paired it
/// with. Column names come from this table — never from the request — which is what
/// makes the generated SQL safe.
#[derive(Clone, Debug, Default)]
pub struct Columns(BTreeMap<String, (String, ColumnKind)>);

impl Columns {
    pub fn new(
        entries: impl IntoIterator<Item = (&'static str, &'static str, ColumnKind)>,
    ) -> Self {
        Self(
            entries
                .into_iter()
                .map(|(path, column, kind)| (path.to_string(), (column.to_string(), kind)))
                .collect(),
        )
    }

    /// When every attribute path is already its column name and every column is text.
    pub fn text(paths: impl IntoIterator<Item = &'static str>) -> Self {
        Self(
            paths
                .into_iter()
                .map(|path| (path.to_string(), (path.to_string(), ColumnKind::Text)))
                .collect(),
        )
    }

    pub fn insert(mut self, path: &str, column: &str, kind: ColumnKind) -> Self {
        self.0.insert(path.to_string(), (column.to_string(), kind));
        self
    }

    /// Every queryable path, for error messages.
    pub fn paths(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    fn resolve(&self, path: &str) -> DomainResult<(&str, ColumnKind)> {
        self.0
            .get(path)
            .map(|(column, kind)| (column.as_str(), *kind))
            .ok_or_else(|| {
                DomainError::invalid(format!(
                    "`{path}` is not queryable; try one of: {}",
                    self.paths().join(", ")
                ))
            })
    }
}

/// SQL fragments built from a [`Criteria`], plus the values to bind.
///
/// The fragments carry their own leading space and are empty when they have nothing to
/// say, so they concatenate directly onto a base statement.
///
/// ```no_run
/// # use jenny_persistence::criteria::SqlCriteria;
/// # fn go(sql: SqlCriteria) {
/// use sqlx::AssertSqlSafe;
///
/// let statement = format!("SELECT id, title FROM notes{}", sql.tail());
///
/// // sqlx 0.9 requires this acknowledgement for any statement built at runtime, and
/// // it is exactly the right question to be asked. The answer here: every identifier
/// // in `tail()` came from the `Columns` table, and every request-supplied value is a
/// // binding below rather than part of the string.
/// let mut query =
///     sqlx::query_as::<sqlx::Postgres, (uuid::Uuid, String)>(AssertSqlSafe(statement));
/// for binding in sql.bindings() {
///     query = query.bind(binding);
/// }
/// # }
/// ```
#[derive(Clone, Debug, Default)]
pub struct SqlCriteria {
    where_clause: String,
    order_by: String,
    page_clause: String,
    bindings: Vec<String>,
}

impl SqlCriteria {
    /// Translate `criteria`, allowing only what `columns` names.
    pub fn build(criteria: &Criteria, columns: &Columns) -> DomainResult<Self> {
        Self::build_from(criteria, columns, 1)
    }

    /// [`build`](Self::build) for a statement that already binds parameters.
    ///
    /// `first_placeholder` is the number the first generated `$n` should take, so a
    /// repository that already bound `$1` for a tenant id passes `2`.
    pub fn build_from(
        criteria: &Criteria,
        columns: &Columns,
        first_placeholder: usize,
    ) -> DomainResult<Self> {
        let mut bindings = Vec::new();
        let mut conditions = Vec::new();
        let mut next = first_placeholder;

        for filter in &criteria.filter {
            let (column, kind) = columns.resolve(filter.path())?;

            if !kind.supports(filter.check()) {
                return Err(DomainError::invalid(format!(
                    "`{}` does not support the `{}` check",
                    filter.path(),
                    filter.check()
                )));
            }
            kind.check_value(filter.path(), filter.value())?;

            let placeholder = format!("${next}{}", kind.cast().unwrap_or_default());
            next += 1;

            let condition = match filter.check() {
                Check::Eq => format!("{column} = {placeholder}"),
                Check::Gt => format!("{column} > {placeholder}"),
                Check::St => format!("{column} < {placeholder}"),
                Check::Begins | Check::Ends | Check::Contains => {
                    format!("{column} ILIKE ${}", next - 1)
                }
            };

            // Wildcards in the *value* are escaped, so a filter of `%` matches a literal
            // percent sign rather than every row in the table.
            bindings.push(match filter.check() {
                Check::Begins => format!("{}%", escape_like(filter.value())),
                Check::Ends => format!("%{}", escape_like(filter.value())),
                Check::Contains => format!("%{}%", escape_like(filter.value())),
                _ => filter.value().to_string(),
            });

            conditions.push(condition);
        }

        let mut orderings = Vec::new();
        for sort in &criteria.sort {
            let (column, _) = columns.resolve(sort.path())?;
            let direction = match sort.ordering() {
                Ordering::Asc => "ASC",
                Ordering::Desc => "DESC",
            };
            orderings.push(format!("{column} {direction}"));
        }

        // The page is numeric and bounded, so it is formatted rather than bound —
        // there is no string from the request anywhere in this fragment.
        let page_clause = if criteria.page.limit() > 0 {
            format!(
                " LIMIT {} OFFSET {}",
                criteria.page.limit(),
                criteria.page.offset()
            )
        } else {
            String::new()
        };

        Ok(Self {
            where_clause: if conditions.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conditions.join(" AND "))
            },
            order_by: if orderings.is_empty() {
                String::new()
            } else {
                format!(" ORDER BY {}", orderings.join(", "))
            },
            page_clause,
            bindings,
        })
    }

    /// `" WHERE …"`, or empty.
    pub fn where_clause(&self) -> &str {
        &self.where_clause
    }

    /// `" ORDER BY …"`, or empty.
    pub fn order_by(&self) -> &str {
        &self.order_by
    }

    /// `" LIMIT n OFFSET m"`, or empty when no page size was asked for.
    pub fn page_clause(&self) -> &str {
        &self.page_clause
    }

    /// Values for the generated placeholders, in order.
    pub fn bindings(&self) -> &[String] {
        &self.bindings
    }

    /// The three fragments concatenated, for the common case of appending them all.
    pub fn tail(&self) -> String {
        format!("{}{}{}", self.where_clause, self.order_by, self.page_clause)
    }
}

/// Escape the wildcards `LIKE` understands, so a value is matched literally.
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Columns {
        Columns::new([
            ("title", "title", ColumnKind::Text),
            ("age", "age", ColumnKind::Integer),
            ("archived", "archived", ColumnKind::Bool),
            ("id", "id", ColumnKind::Uuid),
            ("created_at", "created_at", ColumnKind::Timestamp),
            ("author.name", "a.name", ColumnKind::Text),
        ])
    }

    fn build(query: &str) -> DomainResult<SqlCriteria> {
        let criteria = jenny_rest_free_parse(query)?;
        SqlCriteria::build(&criteria, &columns())
    }

    /// Build a `Criteria` from the compact spellings, without depending on jenny-rest.
    fn jenny_rest_free_parse(query: &str) -> DomainResult<Criteria> {
        let mut criteria = Criteria::new();
        for part in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = part.split_once('=').unwrap();
            match key {
                "sort" => criteria.sort.push(value.parse()?),
                "filter" => criteria.filter.push(value.parse()?),
                other => panic!("unexpected key {other}"),
            }
        }
        Ok(criteria)
    }

    #[test]
    fn nothing_asked_for_produces_nothing() {
        let sql = SqlCriteria::build(&Criteria::new(), &columns()).unwrap();
        assert_eq!(sql.where_clause(), "");
        assert_eq!(sql.order_by(), "");
        assert_eq!(sql.page_clause(), "");
        assert!(sql.bindings().is_empty());
        assert_eq!(sql.tail(), "");
    }

    #[test]
    fn equality_binds_the_value_and_never_interpolates_it() {
        let sql = build("filter=title:eq:milk").unwrap();
        assert_eq!(sql.where_clause(), " WHERE title = $1");
        assert_eq!(sql.bindings(), ["milk"]);
    }

    #[test]
    fn a_path_maps_to_its_configured_column() {
        let sql = build("filter=author.name:eq:bo&sort=author.name").unwrap();
        assert_eq!(sql.where_clause(), " WHERE a.name = $1");
        assert_eq!(sql.order_by(), " ORDER BY a.name ASC");
    }

    #[test]
    fn typed_columns_get_a_cast() {
        assert_eq!(
            build("filter=age:gt:30").unwrap().where_clause(),
            " WHERE age > $1::bigint"
        );
        assert_eq!(
            build("filter=id:eq:a9d2e97a-0f0c-40ff-85ac-1ce6b7a67f3e")
                .unwrap()
                .where_clause(),
            " WHERE id = $1::uuid"
        );
    }

    #[test]
    fn substring_checks_become_escaped_ilike_patterns() {
        assert_eq!(
            build("filter=title:contains:milk").unwrap().bindings(),
            ["%milk%"]
        );
        assert_eq!(build("filter=title:begins:mi").unwrap().bindings(), ["mi%"]);
        assert_eq!(build("filter=title:ends:lk").unwrap().bindings(), ["%lk"]);
        assert_eq!(
            build("filter=title:contains:milk").unwrap().where_clause(),
            " WHERE title ILIKE $1"
        );
    }

    #[test]
    fn wildcards_in_a_value_are_escaped_not_honoured() {
        // Without escaping this would match every row.
        let sql = build("filter=title:contains:%").unwrap();
        assert_eq!(sql.bindings(), ["%\\%%"]);

        let sql = build("filter=title:eq:100%").unwrap();
        assert_eq!(sql.bindings(), ["100%"], "eq is not a pattern match");

        let sql = build("filter=title:begins:a_b").unwrap();
        assert_eq!(sql.bindings(), ["a\\_b%"]);
    }

    #[test]
    fn several_filters_are_anded_with_ascending_placeholders() {
        let sql = build("filter=title:contains:milk&filter=age:gt:30").unwrap();
        assert_eq!(
            sql.where_clause(),
            " WHERE title ILIKE $1 AND age > $2::bigint"
        );
        assert_eq!(sql.bindings(), ["%milk%", "30"]);
    }

    #[test]
    fn placeholders_can_start_past_a_repositorys_own_bindings() {
        let criteria = jenny_rest_free_parse("filter=title:eq:milk&filter=age:gt:1").unwrap();
        let sql = SqlCriteria::build_from(&criteria, &columns(), 3).unwrap();
        assert_eq!(sql.where_clause(), " WHERE title = $3 AND age > $4::bigint");
    }

    #[test]
    fn sorting_renders_both_directions_in_order() {
        let sql = build("sort=-created_at&sort=title").unwrap();
        assert_eq!(sql.order_by(), " ORDER BY created_at DESC, title ASC");
    }

    #[test]
    fn a_page_is_formatted_from_numbers_not_bound() {
        let criteria = Criteria::new().with_page(jenny_core::domain::Page::new(Some(3), Some(20)));
        let sql = SqlCriteria::build(&criteria, &columns()).unwrap();
        assert_eq!(sql.page_clause(), " LIMIT 20 OFFSET 40");
        assert!(sql.bindings().is_empty());
    }

    #[test]
    fn an_unmapped_path_is_refused_and_says_what_is_allowed() {
        let err = build("filter=password_hash:eq:x").unwrap_err();
        assert!(err.to_string().contains("not queryable"), "{err}");
        assert!(err.to_string().contains("title"), "{err}");
        assert!(matches!(err, DomainError::Invalid { .. }));

        assert!(build("sort=password_hash").is_err());
    }

    #[test]
    fn a_value_of_the_wrong_type_fails_at_the_edge_not_in_the_database() {
        let err = build("filter=age:gt:banana").unwrap_err();
        assert!(err.to_string().contains("not a valid value"), "{err}");

        assert!(build("filter=id:eq:not-a-uuid").is_err());
        assert!(build("filter=archived:eq:maybe").is_err());

        assert!(build("filter=age:gt:30").is_ok());
        assert!(build("filter=archived:eq:TRUE").is_ok());
    }

    #[test]
    fn a_check_a_column_cannot_support_is_refused() {
        let err = build("filter=age:contains:3").unwrap_err();
        assert!(err.to_string().contains("does not support"), "{err}");
        assert!(build("filter=archived:gt:true").is_err());
        assert!(build("filter=title:contains:x").is_ok());
    }

    #[test]
    fn no_request_derived_string_ever_reaches_the_statement() {
        // Every hostile shape is stopped somewhere: at path parsing, at the allowlist,
        // or at the type check. None of them appear in a fragment.
        for hostile in [
            "filter=title:eq:'; DROP TABLE notes; --",
            "filter=title:contains:%' OR '1'='1",
            "filter=title:eq:$1",
        ] {
            let sql = build(hostile).unwrap();
            let tail = sql.tail();
            assert!(
                !tail.contains("DROP") && !tail.contains("OR '1'"),
                "value leaked into `{tail}`"
            );
            // The hostile text is a *binding*, where it is inert.
            assert_eq!(sql.bindings().len(), 1);
        }
    }

    #[test]
    fn text_shorthand_maps_paths_to_themselves() {
        let columns = Columns::text(["title", "body"]);
        assert_eq!(columns.paths(), ["body", "title"]);

        let criteria = jenny_rest_free_parse("filter=body:contains:x").unwrap();
        let sql = SqlCriteria::build(&criteria, &columns).unwrap();
        assert_eq!(sql.where_clause(), " WHERE body ILIKE $1");
    }
}
