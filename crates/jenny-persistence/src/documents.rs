//! A [`Store`] that keeps each aggregate as one `jsonb` document.

use std::fmt;
use std::marker::PhantomData;

use async_trait::async_trait;
use jenny_core::domain::{Aggregate, Criteria, DomainError, DomainResult, ValueKind};
use jenny_core::port::store::{Loaded, Store, Version, encode, expect_version};
use serde_json::Value;
use sqlx::{AssertSqlSafe, PgPool};

use crate::criteria::{Columns, SqlCriteria};
use crate::error::RepositoryError;

/// Aggregates of one kind, one row each: `id`, `version`, and the aggregate as `jsonb`.
///
/// This is the store to reach for first. It needs no mapping code and no migration —
/// the table is created before the service serves — and it honours the whole
/// [`Store`] contract: every save is conditional on the version it was read at, and
/// listings sort and filter inside the database.
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS "note" (
///     id         text        PRIMARY KEY,
///     version    bigint      NOT NULL CHECK (version > 0),
///     document   jsonb       NOT NULL,
///     created_at timestamptz NOT NULL DEFAULT now(),
///     updated_at timestamptz NOT NULL DEFAULT now()
/// )
/// ```
///
/// ### Listing
///
/// [`queryable`](Self::queryable) takes the same `(path, kind)` allowlist as the
/// in-memory store. Each path becomes an expression over the document —
/// `author.name` is `(document #>> '{author,name}')`, cast to the column type its
/// [`ValueKind`] names — and filter values are bound parameters, as everywhere else
/// (ADR-010). A path that is filtered or sorted on often deserves an expression index:
///
/// ```sql
/// CREATE INDEX ON "note" ((document #>> '{title}'));
/// ```
///
/// Text comparisons follow the database's collation, and `begins`, `ends` and
/// `contains` are case-insensitive (`ILIKE`). An equality on text is case-sensitive,
/// where the in-memory store's is not.
pub struct PgDocuments<A> {
    pool: PgPool,
    /// Already quoted.
    table: String,
    columns: Columns,
    kind: PhantomData<fn() -> A>,
}

impl<A: Aggregate> PgDocuments<A> {
    /// Usually reached through [`Postgres::documents`](crate::Postgres::documents),
    /// which also makes sure the table exists before the service serves.
    ///
    /// Panics when `table` is not a plain lower-case SQL identifier.
    pub fn new(pool: PgPool, table: &str) -> Self {
        assert!(
            is_identifier(table),
            "`{table}` is not usable as a table name: use lower-case letters, digits and \
             underscores, starting with a letter"
        );
        Self {
            pool,
            table: format!("\"{table}\""),
            columns: Columns::new("id", []),
            kind: PhantomData,
        }
    }

    /// The attribute paths a listing may sort and filter on, and what each holds.
    ///
    /// Panics when a path is not dotted identifiers (`author.name`), because it is
    /// written into the statement — the client's paths are matched against these, and
    /// never written in themselves.
    pub fn queryable<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = (P, ValueKind)>,
        P: AsRef<str>,
    {
        let mut columns = Columns::new("id", []);
        for (path, kind) in paths {
            let path = path.as_ref();
            assert!(
                path.split('.').all(is_field),
                "`{path}` is not a dotted path of field names"
            );
            columns = columns.insert(path, &expression(path, kind), kind);
        }
        self.columns = columns;
        self
    }

    /// The `CREATE TABLE IF NOT EXISTS` this store needs, for a migration of your own
    /// when `persistence.postgres.create_tables` is off.
    pub fn schema(&self) -> String {
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n    \
             id text PRIMARY KEY,\n    \
             version bigint NOT NULL CHECK (version > 0),\n    \
             document jsonb NOT NULL,\n    \
             created_at timestamptz NOT NULL DEFAULT now(),\n    \
             updated_at timestamptz NOT NULL DEFAULT now()\n)",
            self.table
        )
    }

    /// The version currently stored under `key`, if anything is.
    async fn current_version(&self, key: &str) -> DomainResult<Option<Version>> {
        let statement = format!("SELECT version FROM {} WHERE id = $1", self.table);
        let version: Option<i64> = sqlx::query_scalar(AssertSqlSafe(statement))
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        Ok(version.map(stored_version))
    }
}

#[async_trait]
impl<A: Aggregate> Store<A> for PgDocuments<A> {
    async fn find(&self, id: &A::Id) -> DomainResult<Option<Loaded<A>>> {
        let statement = format!("SELECT version, document FROM {} WHERE id = $1", self.table);
        let row: Option<(i64, Value)> = sqlx::query_as(AssertSqlSafe(statement))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        row.map(|(version, document)| Loaded::decode(document, stored_version(version)))
            .transpose()
    }

    async fn insert(&self, aggregate: A) -> DomainResult<Loaded<A>> {
        let key = aggregate.id().to_string();
        let statement = format!(
            "INSERT INTO {} (id, version, document) VALUES ($1, 1, $2) \
             ON CONFLICT (id) DO NOTHING",
            self.table
        );
        let inserted = sqlx::query(AssertSqlSafe(statement))
            .bind(&key)
            .bind(encode(&aggregate)?)
            .execute(&self.pool)
            .await
            .map_err(database)?;
        if inserted.rows_affected() == 0 {
            return Err(DomainError::conflict(format!(
                "{} {key} already exists",
                A::KIND
            )));
        }
        Loaded::written(aggregate, Version::FIRST)
    }

    async fn save(&self, aggregate: Loaded<A>) -> DomainResult<Loaded<A>> {
        let key = aggregate.id().to_string();
        let read_at = aggregate.version();
        let statement = format!(
            "UPDATE {} SET version = version + 1, document = $3, updated_at = now() \
             WHERE id = $1 AND version = $2 RETURNING version",
            self.table
        );
        let saved: Option<i64> = sqlx::query_scalar(AssertSqlSafe(statement))
            .bind(&key)
            .bind(database_version(read_at))
            .bind(aggregate.document()?)
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        match saved {
            Some(version) => Loaded::written(aggregate.into_inner(), stored_version(version)),
            // Nothing matched: either it is gone, or someone saved since it was read.
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
            "DELETE FROM {} WHERE id = $1 AND ($2::bigint IS NULL OR version = $2) \
             RETURNING version, document",
            self.table
        );
        let removed: Option<(i64, Value)> = sqlx::query_as(AssertSqlSafe(statement))
            .bind(&key)
            .bind(expected.map(database_version))
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        match removed {
            Some((version, document)) => {
                Ok(Loaded::<A>::decode(document, stored_version(version))?.into_inner())
            }
            None => match self.current_version(&key).await? {
                None => Err(DomainError::not_found()),
                Some(current) => {
                    expect_version::<A>(id, expected, current)?;
                    // Matched neither way, yet present at the expected version: it was
                    // put back in between. Report what the caller can act on.
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
        let statement = format!("SELECT version, document FROM {}{}", self.table, sql.tail());
        let mut query = sqlx::query_as::<_, (i64, Value)>(AssertSqlSafe(statement));
        for binding in sql.bindings() {
            query = query.bind(binding);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(database)?;
        rows.into_iter()
            .map(|(version, document)| {
                Ok(Loaded::<A>::decode(document, stored_version(version))?.into_inner())
            })
            .collect()
    }
}

impl<A> fmt::Debug for PgDocuments<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgDocuments")
            .field("table", &self.table)
            .field("queryable", &self.columns.paths())
            .finish_non_exhaustive()
    }
}

/// `author.name` as `text` → `(document #>> '{author,name}')`, cast for other kinds.
fn expression(path: &str, kind: ValueKind) -> String {
    let pointer = path.split('.').collect::<Vec<_>>().join(",");
    cast_text(&format!("(document #>> '{{{pointer}}}')"), kind)
}

/// A `text` expression — a value pulled out of `jsonb` — cast to what `kind` holds.
pub(crate) fn cast_text(text: &str, kind: ValueKind) -> String {
    match kind {
        ValueKind::Text => text.to_string(),
        ValueKind::Integer => format!("({text}::bigint)"),
        ValueKind::Float => format!("({text}::double precision)"),
        ValueKind::Bool => format!("({text}::boolean)"),
        ValueKind::Uuid => format!("({text}::uuid)"),
        ValueKind::Timestamp => format!("({text}::timestamptz)"),
    }
}

/// Lower-case letters, digits and underscores, starting with a letter: safe to quote
/// and use as a table name without surprises about case folding.
pub(crate) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 63
}

/// A JSON field name safe to write into a `#>>` path literal.
pub(crate) fn is_field(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn stored_version(version: i64) -> Version {
    // The column has `CHECK (version > 0)`, so the cast cannot lose anything.
    Version::new(version as u64)
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
    fn paths_become_typed_expressions_over_the_document() {
        assert_eq!(
            expression("author.name", ValueKind::Text),
            "(document #>> '{author,name}')"
        );
        assert_eq!(
            expression("priority", ValueKind::Integer),
            "((document #>> '{priority}')::bigint)"
        );
    }

    #[test]
    fn only_plain_identifiers_are_table_names() {
        assert!(is_identifier("note"));
        assert!(is_identifier("order_line2"));
        for bad in ["", "Note", "1note", "note;drop", "no te", "\"note\""] {
            assert!(!is_identifier(bad), "{bad}");
        }
        assert!(!is_field("a'b"));
        assert!(!is_field(""));
    }
}
