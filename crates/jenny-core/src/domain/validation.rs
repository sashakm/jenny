//! Business validation the domain owns.
//!
//! There are two kinds of validation in a hexagonal service and they live in
//! different places:
//!
//! - **Format validation** — "is this even a well-formed request?" — belongs to the
//!   inbound adapter. In jenny that is the `TryFrom<Dto> for Command` impl in
//!   `adapter/inbound/…`, which is per-protocol by design.
//! - **Business validation** — "does this break a rule?" — belongs to the domain and
//!   is what [`Validate`] is for. Write it once and every protocol enforces it,
//!   because the rule travels with the type rather than with the endpoint.
//!
//! Where a rule can be encoded in the type system instead, prefer that: a newtype
//! with a smart constructor makes the invalid state unrepresentable, and then no
//! validation pass is needed at all. [`Validate`] is for the rules that span fields
//! or need a readable report of everything that was wrong at once.
//!
//! ```
//! use jenny_core::domain::{DomainError, Validate, ValidationErrors};
//!
//! struct CreateNote { title: String, body: String }
//!
//! impl Validate for CreateNote {
//!     fn validate(&self) -> Result<(), ValidationErrors> {
//!         let mut errors = ValidationErrors::default();
//!         if self.title.trim().is_empty() {
//!             errors.push_field("title", "must not be empty");
//!         }
//!         if self.body.len() > 10_000 {
//!             errors.push_field("body", "must be at most 10000 characters");
//!         }
//!         errors.into_result()
//!     }
//! }
//!
//! let note = CreateNote { title: "  ".into(), body: String::new() };
//! let error: DomainError = note.validate().unwrap_err().into();
//! assert!(matches!(error, DomainError::Invalid { .. }));
//! ```

use std::fmt::Display;

use super::error::DomainError;

/// One thing that was wrong, optionally pinned to a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationIssue {
    pub field: Option<String>,
    pub message: String,
}

impl Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.field {
            Some(field) => write!(f, "{field}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

/// Everything that was wrong, collected in one pass.
///
/// Collecting rather than short-circuiting is deliberate: a caller fixing a form
/// wants the whole list, not the first problem repeated five times.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidationErrors(Vec<ValidationIssue>);

impl ValidationErrors {
    /// Record an issue about the value as a whole.
    pub fn push(&mut self, message: impl Into<String>) {
        self.0.push(ValidationIssue {
            field: None,
            message: message.into(),
        });
    }

    /// Record an issue about one field.
    pub fn push_field(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.0.push(ValidationIssue {
            field: Some(field.into()),
            message: message.into(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn issues(&self) -> &[ValidationIssue] {
        &self.0
    }

    /// `Ok(())` when nothing was recorded, `Err(self)` otherwise. The idiomatic last
    /// line of a [`Validate::validate`] implementation.
    pub fn into_result(self) -> Result<(), Self> {
        if self.is_empty() { Ok(()) } else { Err(self) }
    }
}

impl Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rendered: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        write!(f, "{}", rendered.join("; "))
    }
}

impl From<ValidationErrors> for DomainError {
    fn from(errors: ValidationErrors) -> Self {
        DomainError::invalid(errors.to_string())
    }
}

/// A type that knows its own domain rules.
pub trait Validate {
    fn validate(&self) -> Result<(), ValidationErrors>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Note {
        title: String,
    }

    impl Validate for Note {
        fn validate(&self) -> Result<(), ValidationErrors> {
            let mut errors = ValidationErrors::default();
            if self.title.is_empty() {
                errors.push_field("title", "must not be empty");
            }
            if self.title.len() > 8 {
                errors.push("note is too large");
            }
            errors.into_result()
        }
    }

    #[test]
    fn a_valid_value_passes() {
        assert!(Note { title: "ok".into() }.validate().is_ok());
    }

    #[test]
    fn issues_render_with_their_field() {
        let errors = Note { title: "".into() }.validate().unwrap_err();
        assert_eq!(errors.issues().len(), 1);
        assert_eq!(errors.to_string(), "title: must not be empty");
    }

    #[test]
    fn every_issue_is_collected_not_just_the_first() {
        let errors = Note {
            title: String::new(),
        }
        .validate()
        .unwrap_err();
        assert_eq!(errors.issues().len(), 1);

        let long = Note {
            title: "waaaaay too long".into(),
        }
        .validate()
        .unwrap_err();
        assert_eq!(long.issues().len(), 1);
        let error: DomainError = long.into();
        assert!(matches!(error, DomainError::Invalid { .. }));
        assert!(error.to_string().contains("note is too large"));
    }
}
