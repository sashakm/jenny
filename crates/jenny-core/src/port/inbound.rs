//! Driving ports — what the outside world is allowed to ask the application to do.
//!
//! ### You often do not need a trait
//!
//! A use case is a unit of application behaviour. In Rust that is usually just a
//! function, and a function is the right default:
//!
//! ```
//! # use jenny_core::domain::{DomainError, DomainResult};
//! # use jenny_core::port::Context;
//! # struct Note; struct NoteId;
//! # trait NoteRepository: Send + Sync { fn insert(&self, note: Note) -> DomainResult<NoteId>; }
//! pub struct CreateNote { pub title: String }
//!
//! pub fn create_note<R: NoteRepository>(
//!     repo: &R,
//!     cmd: CreateNote,
//!     cx: &Context,
//! ) -> DomainResult<NoteId> {
//!     repo.insert(Note)
//! }
//! ```
//!
//! Static dispatch, no allocation, trivially testable with a fake repository. Reach
//! for [`UseCase`] when you actually need the indirection: when several driving
//! adapters must be decoupled from the implementation, when you want to store a
//! heterogeneous set of use cases behind `Arc<dyn …>`, or when a decorator
//! (retry, cache, audit) should wrap any of them uniformly.
//!
//! ### The trait, when you do want it
//!
//! ```
//! use async_trait::async_trait;
//! use jenny_core::domain::{DomainError, Validate};
//! use jenny_core::port::{Context, UseCase};
//!
//! # #[derive(Default)] struct NoteStore;
//! # impl NoteStore { async fn insert(&self, _: &str) -> Result<uuid::Uuid, DomainError> { Ok(uuid::Uuid::nil()) } }
//! pub struct CreateNote { store: NoteStore }
//!
//! pub struct CreateNoteCommand { pub title: String }
//!
//! #[async_trait]
//! impl UseCase for CreateNote {
//!     type Input = CreateNoteCommand;
//!     type Output = uuid::Uuid;
//!     type Error = DomainError;
//!
//!     async fn invoke(&self, cmd: Self::Input, cx: &Context) -> Result<Self::Output, Self::Error> {
//!         tracing::debug!(request_id = %cx.request_id(), "creating note");
//!         self.store.insert(&cmd.title).await
//!     }
//! }
//! ```

use async_trait::async_trait;

use crate::domain::DomainError;

use super::context::Context;

/// One unit of application behaviour, reachable from any driving adapter.
///
/// The input is a domain command or query — not a DTO, not an HTTP body. Translating
/// the wire format into it is the inbound adapter's job, which is exactly what keeps
/// this trait free of transport concerns: REST, gRPC, a message consumer and a
/// scheduled task all converge on the same `Input` type.
///
/// `Error` must convert into [`DomainError`] so that adapters have a status mapping
/// to fall back on, but a use case is free to return a narrower, more precise error
/// of its own and let the conversion happen at the boundary.
#[async_trait]
pub trait UseCase: Send + Sync + 'static {
    /// The domain command or query this use case accepts.
    type Input: Send;
    /// What it produces on success.
    type Output: Send;
    /// How it fails.
    type Error: Into<DomainError> + Send;

    async fn invoke(&self, input: Self::Input, cx: &Context) -> Result<Self::Output, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Principal;

    struct Greet;

    #[async_trait]
    impl UseCase for Greet {
        type Input = String;
        type Output = String;
        type Error = DomainError;

        async fn invoke(
            &self,
            name: Self::Input,
            cx: &Context,
        ) -> Result<Self::Output, Self::Error> {
            let who = cx.principal().map(|p| p.id().to_string());
            Ok(match who {
                Some(caller) => format!("Hello, {name}! (asked by {caller})"),
                None => format!("Hello, {name}!"),
            })
        }
    }

    struct AlwaysMissing;

    #[async_trait]
    impl UseCase for AlwaysMissing {
        type Input = ();
        type Output = ();
        type Error = DomainError;

        async fn invoke(&self, _: Self::Input, _: &Context) -> Result<Self::Output, Self::Error> {
            Err(DomainError::not_found())
        }
    }

    #[tokio::test]
    async fn a_use_case_reads_ambient_facts_from_the_context() {
        let cx = Context::new();
        assert_eq!(
            Greet.invoke("world".into(), &cx).await.unwrap(),
            "Hello, world!"
        );

        let cx = Context::new().with_principal(Principal::builder("u-1").build());
        assert_eq!(
            Greet.invoke("world".into(), &cx).await.unwrap(),
            "Hello, world! (asked by u-1)"
        );
    }

    #[tokio::test]
    async fn failures_surface_as_domain_errors() {
        let err = AlwaysMissing.invoke((), &Context::new()).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound));
    }

    #[tokio::test]
    async fn use_cases_can_be_erased_behind_a_trait_object() {
        let erased: std::sync::Arc<
            dyn UseCase<Input = String, Output = String, Error = DomainError>,
        > = std::sync::Arc::new(Greet);
        assert_eq!(
            erased.invoke("dyn".into(), &Context::new()).await.unwrap(),
            "Hello, dyn!"
        );
    }
}
