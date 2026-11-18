//! The unit the application loads, changes and saves as a whole.

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A consistency boundary: one entity, with whatever it owns, that is always loaded,
/// changed and saved together.
///
/// The trait asks for an identity and a kind, and for serde. Serde is what lets a
/// [`Store`](crate::port::Store) persist any aggregate without per-type mapping code,
/// and ADR-012 explains why it is allowed this far in. The invariants survive it:
/// value objects deserialise through their smart constructors, so stored data that
/// breaks a rule fails to load rather than producing an aggregate in a state the
/// domain would never have built.
///
/// ```
/// use jenny_core::domain::{Aggregate, DomainError};
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// #[serde(transparent)]
/// pub struct NoteId(uuid::Uuid);
///
/// impl std::fmt::Display for NoteId {
///     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
///         self.0.fmt(f)
///     }
/// }
///
/// /// Loading a stored title runs this constructor again.
/// #[derive(Clone, Debug, Serialize, Deserialize)]
/// #[serde(try_from = "String", into = "String")]
/// pub struct Title(String);
///
/// impl TryFrom<String> for Title {
///     type Error = DomainError;
///     fn try_from(raw: String) -> Result<Self, DomainError> {
///         if raw.trim().is_empty() {
///             return Err(DomainError::invalid("title must not be empty"));
///         }
///         Ok(Self(raw))
///     }
/// }
///
/// impl From<Title> for String {
///     fn from(title: Title) -> String { title.0 }
/// }
///
/// #[derive(Serialize, Deserialize)]
/// pub struct Note { id: NoteId, title: Title }
///
/// impl Aggregate for Note {
///     type Id = NoteId;
///     const KIND: &'static str = "note";
///     fn id(&self) -> NoteId { self.id }
/// }
/// ```
///
/// `#[derive(Aggregate)]` writes that impl for you.
pub trait Aggregate: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// The identity. Its `Display` form is the key the aggregate is stored under, so it
    /// must be unique per kind and must not change.
    type Id: Clone + fmt::Display + Send + Sync + 'static;

    /// What this is, in the singular: `"note"`, `"order"`. Stores name their table,
    /// collection or bucket after it, and change events are called `<kind>.updated`.
    const KIND: &'static str;

    fn id(&self) -> Self::Id;
}
