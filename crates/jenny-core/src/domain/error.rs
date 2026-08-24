//! The domain failure vocabulary.

use thiserror::Error;

use super::identity::{AuthenticationError, AuthorizationError};

/// What went wrong, phrased in domain terms rather than protocol terms.
///
/// A use case returns this (or an error convertible into it); each inbound adapter
/// translates it into its own protocol's failure representation. The variants are
/// deliberately few: they are the categories that every protocol can express.
///
/// | Variant | HTTP | gRPC |
/// |---|---|---|
/// | [`NotFound`](Self::NotFound) | 404 | `NOT_FOUND` |
/// | [`Invalid`](Self::Invalid) | 400 | `INVALID_ARGUMENT` |
/// | [`Unauthenticated`](Self::Unauthenticated) | 401 | `UNAUTHENTICATED` |
/// | [`Forbidden`](Self::Forbidden) | 403 | `PERMISSION_DENIED` |
/// | [`Conflict`](Self::Conflict) | 409 | `ABORTED` |
/// | [`Internal`](Self::Internal) | 500 | `INTERNAL` |
#[derive(Debug, Error)]
pub enum DomainError {
    /// The addressed thing does not exist.
    #[error("not found")]
    NotFound,

    /// The input broke a domain rule or invariant.
    #[error("invalid: {detail}")]
    Invalid { detail: String },

    /// The caller could not be identified.
    #[error("unauthenticated: {detail}")]
    Unauthenticated { detail: String },

    /// The caller is known but may not do this.
    #[error("forbidden: {detail}")]
    Forbidden { detail: String },

    /// The operation conflicts with the current state — a duplicate, or a stale
    /// version in an optimistic-concurrency check.
    #[error("conflict: {detail}")]
    Conflict { detail: String },

    /// Something failed that the caller can do nothing about.
    #[error("internal error: {detail}")]
    Internal { detail: String },
}

impl DomainError {
    pub fn not_found() -> Self {
        Self::NotFound
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Invalid {
            detail: detail.into(),
        }
    }

    pub fn unauthenticated(detail: impl Into<String>) -> Self {
        Self::Unauthenticated {
            detail: detail.into(),
        }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::Forbidden {
            detail: detail.into(),
        }
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::Conflict {
            detail: detail.into(),
        }
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }
}

/// The result a use case produces unless it defines a narrower error type.
pub type DomainResult<T> = Result<T, DomainError>;

impl From<AuthenticationError> for DomainError {
    fn from(error: AuthenticationError) -> Self {
        Self::unauthenticated(error.to_string())
    }
}

impl From<AuthorizationError> for DomainError {
    fn from(error: AuthorizationError) -> Self {
        Self::forbidden(error.to_string())
    }
}

impl From<anyhow::Error> for DomainError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_pick_the_right_variant() {
        assert!(matches!(DomainError::not_found(), DomainError::NotFound));
        assert!(matches!(
            DomainError::conflict("duplicate title"),
            DomainError::Conflict { .. }
        ));
        assert!(
            DomainError::invalid("missing field 'title'")
                .to_string()
                .contains("missing field 'title'")
        );
    }

    #[test]
    fn authentication_failure_maps_to_unauthenticated() {
        let err: DomainError = AuthenticationError("bad token".into()).into();
        assert!(matches!(err, DomainError::Unauthenticated { .. }));
    }

    #[test]
    fn authorization_failure_maps_to_forbidden() {
        let err: DomainError = AuthorizationError("nope".into()).into();
        assert!(matches!(err, DomainError::Forbidden { .. }));
    }

    #[test]
    fn anyhow_maps_to_internal() {
        let err: DomainError = anyhow::anyhow!("boom").into();
        assert!(matches!(err, DomainError::Internal { .. }));
        assert!(err.to_string().contains("boom"));
    }
}
