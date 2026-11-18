use jenny_core::domain::DomainError;
use thiserror::Error;

/// An error class for handling persistence layer specific failures
#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("Could not find a matching database entry")]
    DatabaseLookupError(),
    #[error("Database error: {0}")]
    DatabaseError(#[source] sqlx::Error),
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
/// nothing about, so it must not be reported as their mistake. The repository error
/// stays attached as the source, so the database's own message reaches the logs.
impl From<RepositoryError> for DomainError {
    fn from(error: RepositoryError) -> Self {
        match error {
            RepositoryError::DatabaseLookupError() => Self::not_found(),
            other => Self::internal_from(other),
        }
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
    fn a_lookup_miss_becomes_not_found() {
        let err: DomainError = RepositoryError::DatabaseLookupError().into();
        assert!(matches!(err, DomainError::NotFound));
    }

    #[test]
    fn any_other_database_failure_becomes_internal_not_the_callers_fault() {
        let err: DomainError = RepositoryError::GenericRepositoryError("oops").into();
        assert!(matches!(
            err,
            DomainError::Internal {
                source: Some(_),
                ..
            }
        ));
        assert!(err.to_string().contains("oops"));
    }

    #[test]
    fn the_database_message_survives_into_the_report() {
        let err: DomainError =
            RepositoryError::from(sqlx::Error::Protocol("server closed the connection".into()))
                .into();
        assert!(
            err.report().contains("server closed the connection"),
            "{}",
            err.report()
        );
    }
}
