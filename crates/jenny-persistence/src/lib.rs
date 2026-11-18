//! Postgres for jenny services: a store that needs no mapping code, and the building
//! blocks for ports of your own.
//!
//! ### A store from configuration
//!
//! [`Postgres`] is a lazily connecting pool built from the `persistence.postgres`
//! section ([`PostgresConfig`]). [`Postgres::documents`] hands out a [`PgDocuments`], a
//! [`Store<A>`](jenny_core::port::Store) that keeps each aggregate as one `jsonb` row
//! and needs nothing but the aggregate's serde derives; [`Postgres::table`] hands out a
//! [`PgTable`] over a table of your own with a column per field. The runtime builds the
//! pool, creates the tables, runs your migrations and registers a readiness check. With
//! [`Postgres::with_outbox`] — `persistence.postgres.outbox: true` — events raised with a
//! change are committed in the same transaction, and an [`OutboxRelay`] forwards them:
//!
//! ```ignore
//! // in `build`, the composition root
//! let notes: Arc<NoteStore> = match server.postgres()? {
//!     Some(postgres) => Arc::new(postgres.documents::<Note>().queryable(QUERYABLE)),
//!     None => Arc::new(Memory::<Note>::new().queryable(QUERYABLE)),
//! };
//! ```
//!
//! ### Ports of your own
//!
//! Queries phrased in your domain's language go on a port that extends `Store<A>`
//! (ADR-012), implemented by hand against the pool. What this crate offers that
//! implementation: client criteria turned into SQL safely ([`SqlCriteria`] with a
//! [`Columns`] allowlist), column lists for composite rows ([`QueryFormat`]), and
//! [`RepositoryError`], which converts into a `DomainError` so a database failure
//! arrives in the application layer already phrased as a domain failure.
//!
//! ```text
//! domain/port/         NoteRepository: Store<Note>   (trait — yours)
//!      ▲
//! adapter/outbound/persistence/                      (impl — uses this crate)
//!      └─ PgNoteRepository: sqlx, SqlCriteria, RepositoryError -> DomainError
//! ```
pub mod criteria;
mod documents;
mod error;
pub mod health;
mod outbox;
mod postgres;
mod query_format;
mod table;

#[doc(inline)]
pub use criteria::{Columns, SqlCriteria};
#[doc(inline)]
pub use documents::PgDocuments;
#[doc(inline)]
pub use error::*;
/// What a column holds — the core's [`ValueKind`], the same
/// one an in-memory store's allowlist uses.
pub use jenny_core::domain::ValueKind;
pub use jenny_macros::QueryFormat;
#[doc(inline)]
pub use outbox::OutboxRelay;
#[doc(inline)]
pub use postgres::{Postgres, PostgresConfig};
#[doc(inline)]
pub use query_format::{QueryFormat, QueryFormatExt};
/// The driver underneath, for a port implemented by hand against [`Postgres::pool`].
pub use sqlx;
#[doc(inline)]
pub use table::PgTable;
