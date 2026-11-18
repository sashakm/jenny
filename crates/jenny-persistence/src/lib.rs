//! Building blocks for driven persistence adapters. Postgres, for now.
//!
//! This crate deliberately does **not** define a `Repository` trait. The repository
//! port belongs to your domain — `NoteRepository`, returning `Note` and
//! [`DomainError`](jenny_core::domain::DomainError), phrased in the language your
//! aggregates use — and any generic `Repository<T, Id>` the framework invented would
//! be a worse version of it that leaks storage concepts back inward.
//!
//! What lives here is the machinery an implementation of *your* port needs on the
//! other side of that boundary: turning client criteria into SQL safely
//! ([`SqlCriteria`] with a [`Columns`] allowlist), column lists for composite rows
//! ([`QueryFormat`]), a [`DatabaseCheck`](health::DatabaseCheck), and the error type,
//! with a `From` impl so a repository failure arrives in the application layer already
//! phrased as a domain failure.
//!
//! ```text
//! domain/port/         NoteRepository            (trait — yours)
//!      ▲
//! adapter/outbound/persistence/                  (impl — uses this crate)
//!      └─ PgNoteRepository: sqlx, QueryFormat, RepositoryError -> DomainError
//! ```
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
