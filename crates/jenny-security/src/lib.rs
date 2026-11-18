//! Driven adapters for the security ports.
//!
//! The ports themselves — [`Authenticator`] and [`Authorizer`] — are owned by the
//! core, because the application needs to *ask* those questions without caring how
//! they are answered. This crate supplies the answering machinery:
//!
//! | Port | Implementation here |
//! |---|---|
//! | [`Authenticator`] | [`AuthenticationConfig`] — OIDC/JWT verification against a JWKS, and hashed API-key lookup |
//! | [`Authorizer`] | [`CedarAuthorizer`] — [Cedar](https://www.cedarpolicy.com) policies, optionally validated against a schema |
//!
//! Nothing in here knows about HTTP. Lifting credentials off a request is the inbound
//! adapter's job — `jenny-rest` does it for HTTP — and this crate takes the
//! [`Credentials`] that produced and decides whether they are real.
//!
//! [`Authenticator`]: jenny_core::port::outbound::Authenticator
//! [`Authorizer`]: jenny_core::port::outbound::Authorizer
//! [`Credentials`]: jenny_core::domain::Credentials

pub mod authentication;
pub mod authorization;

#[doc(inline)]
pub use authentication::*;
#[doc(inline)]
pub use authorization::*;

// Re-exported so that wiring code needs one `use jenny_security::…` rather than two.
pub use jenny_core::domain::{
    AuthenticationError, AuthorizationError, Credentials, Principal, PrincipalBuilder,
};
pub use jenny_core::port::outbound::{AccessRequest, Authenticator, Authorizer, Resource};

mod utils;
