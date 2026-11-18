//! The domain failure vocabulary.

use thiserror::Error;

use super::identity::{AuthenticationError, AuthorizationError};
use super::validation::{ValidationErrors, ValidationIssue};

/// What caused an [`Internal`](DomainError::Internal) failure, kept for the logs.
pub type ErrorSource = Box<dyn std::error::Error + Send + Sync + 'static>;

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

    /// The input broke a domain rule or invariant. `issues` lists what was wrong field
    /// by field when the failure came from [`Validate`](super::Validate), so an adapter
    /// can report each one; it is empty otherwise.
    #[error("invalid: {detail}")]
    Invalid {
        detail: String,
        issues: Vec<ValidationIssue>,
    },

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

    /// Something failed that the caller can do nothing about. `source` is the error
    /// behind it, so the whole chain reaches the logs even though none of it reaches
    /// the caller.
    #[error("internal error: {detail}")]
    Internal {
        detail: String,
        #[source]
        source: Option<ErrorSource>,
    },
}

impl DomainError {
    pub fn not_found() -> Self {
        Self::NotFound
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Invalid {
            detail: detail.into(),
            issues: Vec::new(),
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
            source: None,
        }
    }

    /// An internal failure caused by `source`, whose message becomes the detail.
    pub fn internal_from(source: impl Into<ErrorSource>) -> Self {
        let source = source.into();
        Self::Internal {
            detail: source.to_string(),
            source: Some(source),
        }
    }

    /// The message and every cause below it, `outer: inner: innermost`. What an
    /// adapter logs for an internal failure.
    pub fn report(&self) -> String {
        let mut report = self.to_string();
        let mut cause = std::error::Error::source(self);
        while let Some(error) = cause {
            let message = error.to_string();
            // `anyhow` repeats its own context in `Display`; skip what is already there.
            if !report.ends_with(&message) {
                report.push_str(": ");
                report.push_str(&message);
            }
            cause = error.source();
        }
        report
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
        Self::internal_from(error)
    }
}

impl From<ValidationErrors> for DomainError {
    fn from(errors: ValidationErrors) -> Self {
        Self::Invalid {
            detail: errors.to_string(),
            issues: errors.issues().to_vec(),
        }
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
    fn anyhow_maps_to_internal_and_keeps_the_chain() {
        let root = std::io::Error::other("connection refused");
        let err: DomainError = anyhow::Error::new(root).context("loading note 7").into();
        assert!(matches!(
            err,
            DomainError::Internal {
                source: Some(_),
                ..
            }
        ));
        assert!(err.to_string().contains("loading note 7"));
        assert_eq!(
            err.report(),
            "internal error: loading note 7: connection refused"
        );
    }

    #[test]
    fn validation_errors_keep_their_issues() {
        let mut errors = ValidationErrors::default();
        errors.push_field("title", "must not be empty");
        errors.push("too large");
        let DomainError::Invalid { detail, issues } = errors.into() else {
            panic!("expected Invalid");
        };
        assert_eq!(detail, "title: must not be empty; too large");
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].field.as_deref(), Some("title"));
    }
}
