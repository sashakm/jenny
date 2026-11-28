//! The hexagon: types a domain module may depend on.
//!
//! Everything here is free of infrastructure: no database driver, no HTTP or broker
//! type. Serde is the one exception, and a deliberate one — aggregates and events
//! derive it so stores and publishers need no per-type mapping (ADR-012). A `domain/`
//! module in a jenny service should need imports from this module and from
//! [`crate::port`] only, plus `serde`.
//!
//! | Module | Holds |
//! |---|---|
//! | [`aggregate`] | [`Aggregate`] — the unit a store loads and saves |
//! | [`changes`] | [`Changes`] — what changed between two states, as a JSON Patch |
//! | [`error`] | [`DomainError`] — the failure vocabulary every adapter maps to a protocol status |
//! | [`event`] | [`DomainEvent`] — the marker for event types defined in `domain/event/` |
//! | [`identity`] | [`Principal`], [`Credentials`] and the two authentication/authorization failures |
//! | [`label`] | [`Urn`] and [`Label`] — identity and categorisation of resources |
//! | [`query`] | [`Criteria`] — transport-neutral paging, sorting and filtering |
//! | [`validation`] | [`Validate`] — business/invariant validation, reusable by every adapter |
//!
//! Input *format* validation belongs in the inbound adapter (a `TryFrom` from a
//! per-protocol DTO to a domain command). [`Validate`] is for the rules the domain
//! owns, so they hold no matter which protocol the call arrived on.

pub mod aggregate;
pub mod changes;
pub mod error;
pub mod event;
pub mod identity;
pub mod label;
pub mod query;
pub mod validation;

#[doc(inline)]
pub use aggregate::Aggregate;
#[doc(inline)]
pub use changes::Changes;
#[doc(inline)]
pub use error::{DomainError, DomainResult, ErrorSource};
#[doc(inline)]
pub use event::DomainEvent;
#[doc(inline)]
pub use identity::{
    AuthenticationError, AuthorizationError, Credentials, Principal, PrincipalBuilder,
    PrincipalKind,
};
#[doc(inline)]
pub use label::{Label, Urn};
#[doc(inline)]
pub use query::{
    Check, Criteria, DEFAULT_PAGE_SIZE, Filter, MAX_PAGE_SIZE, Ordering, Page, Sort, ValueKind,
};
#[doc(inline)]
pub use validation::{Validate, ValidationErrors, ValidationIssue};
