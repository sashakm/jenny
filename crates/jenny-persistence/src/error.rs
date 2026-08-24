use jenny_core::domain::DomainError;
use thiserror::Error;

/// Type for errors during type-to-statement mapping operations
#[derive(Debug, Error)]
pub enum MappingError {
    #[error("Failure while serializing mapping data: {0}")]
    InputDataError(#[from] serde_json::Error),
    #[error("Failure while constructing statement: {0}")]
    BuildError(String),
    #[error("Failure while collecting statement arguments: {0}")]
    ArgumentError(String),
    #[error("Could not map row to requested type")]
    InputRowError(#[from] sqlx::Error),
}

/// An error class for handling persistence layer specific failures
#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("Could not find a matching database entry")]
    DatabaseLookupError(),
    #[error("Database error: {0}")]
    DatabaseError(sqlx::Error),
    #[error("SQL Statement Error: {0}")]
    SqlStatementError(#[from] MappingError),
    #[error("Repository failure: {0}")]
    GenericRepositoryError(&'static str),
}

impl From<sqlx::Error> for RepositoryError {
    fn from(value: sqlx::Error) -> Self {
        match value {
            sqlx::Error::RowNotFound => Self::DatabaseLookupError(),
            _ => RepositoryError::DatabaseError(value),
        }
    }
}

/// A miss is a domain-level "not found"; anything else is a failure the caller can do
/// nothing about, so it must not be reported as their mistake.
impl From<RepositoryError> for DomainError {
    fn from(error: RepositoryError) -> Self {
        match error {
            RepositoryError::DatabaseLookupError() => Self::not_found(),
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<MappingError> for DomainError {
    fn from(error: MappingError) -> Self {
        Self::invalid(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlx_error_to_repository_error_conversion() {
        let repo_err = RepositoryError::from(sqlx::Error::RowNotFound);
        assert_eq!(
            repo_err.to_string(),
            "Could not find a matching database entry".to_owned()
        );
    }

    #[test]
    fn test_mapping_error_to_repository_error_conversion() {
        let repo_err = RepositoryError::from(MappingError::BuildError("Test".to_owned()));
        assert_eq!(
            repo_err.to_string(),
            "SQL Statement Error: Failure while constructing statement: Test"
        )
    }

    #[test]
    fn a_lookup_miss_becomes_not_found() {
        let err: DomainError = RepositoryError::DatabaseLookupError().into();
        assert!(matches!(err, DomainError::NotFound));
    }

    #[test]
    fn any_other_database_failure_becomes_internal_not_the_callers_fault() {
        let err: DomainError = RepositoryError::GenericRepositoryError("oops").into();
        assert!(matches!(err, DomainError::Internal { .. }));
        assert!(err.to_string().contains("oops"));
    }

    #[test]
    fn a_mapping_failure_becomes_invalid() {
        let err: DomainError = MappingError::BuildError("bad".into()).into();
        assert!(matches!(err, DomainError::Invalid { .. }));
    }
}
