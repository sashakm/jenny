//! The hexagon: types a domain module may depend on.
//!
//! Everything here is free of infrastructure. There is no serde derive on an entity,
//! no database driver, no HTTP or broker type. A `domain/` module in a jenny service
//! should need imports from this module and from [`crate::port`] only.
//!
//! | Module | Holds |
//! |---|---|
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

pub mod error;
pub mod event;
pub mod identity;
pub mod label;
pub mod query;
pub mod validation;

#[doc(inline)]
pub use error::{DomainError, DomainResult};
#[doc(inline)]
pub use event::DomainEvent;
#[doc(inline)]
pub use identity::{
    AuthenticationError, AuthorizationError, Credentials, Principal, PrincipalBuilder,
};
#[doc(inline)]
pub use label::{Label, Urn};
#[doc(inline)]
pub use query::{Check, Criteria, Filter, Ordering, Page, Sort};
#[doc(inline)]
pub use validation::{Validate, ValidationErrors, ValidationIssue};
