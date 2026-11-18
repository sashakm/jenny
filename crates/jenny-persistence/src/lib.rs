//! Building blocks for driven persistence adapters. Postgres, for now.
//!
//! The storage port is [`Store<A>`](jenny_core::port::Store), from the core: load by
//! id, save with a version check. Queries phrased in your domain's own language go on
//! a port of yours that extends it, and their implementation is what this crate helps
//! with on the other side of that boundary (ADR-012):
//!
//! ```text
//! domain/port/         NoteRepository: Store<Note>   (trait — yours)
//!      ▲
//! adapter/outbound/persistence/                      (impl — uses this crate)
//!      └─ PgNoteRepository: sqlx, SqlCriteria, RepositoryError -> DomainError
//! ```
//!
//! What lives here: turning client criteria into SQL safely ([`SqlCriteria`] with a
//! [`Columns`] allowlist), column lists for composite rows ([`QueryFormat`]), a
//! [`DatabaseCheck`](health::DatabaseCheck), and the error type, with a `From` impl so
//! a repository failure arrives in the application layer already phrased as a domain
//! failure.
pub mod criteria;
mod error;
pub mod health;
mod query_format;

#[doc(inline)]
pub use criteria::{ColumnKind, Columns, SqlCriteria};
#[doc(inline)]
pub use error::*;
pub use jenny_macros::QueryFormat;
#[doc(inline)]
pub use query_format::{QueryFormat, QueryFormatExt};
