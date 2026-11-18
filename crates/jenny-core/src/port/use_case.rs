//! Driving ports — what the outside world may ask the application to do.
//!
//! A use case is a method on a plain struct that holds the outbound ports it needs.
//! There is no trait to implement: every driving adapter — a REST handler, a message
//! consumer, a scheduled task — calls the method with a domain command and a
//! [`Context`](super::Context), and the struct is concrete, so nothing has to be erased
//! to share it between them.
//!
//! ```
//! use std::sync::Arc;
//! use jenny_core::domain::DomainResult;
//! use jenny_core::port::{Context, Events, Store, StoreExt};
//! # #[derive(serde::Serialize, serde::Deserialize)] pub struct Note { id: u32, archived: bool }
//! # impl jenny_core::domain::Aggregate for Note { type Id = u32; const KIND: &'static str = "note"; fn id(&self) -> u32 { self.id } }
//! # #[derive(serde::Serialize)] pub struct NoteArchived { id: u32 }
//! # impl jenny_core::domain::DomainEvent for NoteArchived { fn name(&self) -> &str { "note.archived" } }
//! # impl Note { fn archive(&mut self) -> DomainResult<NoteArchived> { self.archived = true; Ok(NoteArchived { id: self.id }) } }
//!
//! #[derive(Clone)]
//! pub struct Notes {
//!     notes: Arc<dyn Store<Note>>,
//!     events: Events,
//! }
//!
//! impl Notes {
//!     pub async fn archive(&self, id: u32, cx: &Context) -> DomainResult<()> {
//!         let archived = self.notes.change(&id, Note::archive).await?;
//!         self.events.publish(&archived.outcome, cx).await;
//!         Ok(())
//!     }
//! }
//! ```
//!
//! Put `#[jenny::use_cases]` on that `impl` block and every `pub async fn` in it gets a
//! `use_case` span named `notes.archive` and a `jenny.usecase.duration` measurement
//! labelled with its outcome, through [`observe`](__private::observe). The attribute
//! wraps method bodies and nothing else: it generates no ports, adapters or wiring, so
//! the dependency graph is still the struct's fields (ADR-012).

use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tracing::Instrument;
use uuid::Uuid;

use crate::domain::DomainError;

/// Receives one measurement per finished use case. The runtime installs one that
/// records the `jenny.usecase.duration` histogram; tests and hand-rolled hosts may
/// install their own.
///
/// A process-wide hook rather than a port on each service, because it is the telemetry
/// backend's business, like the `tracing` subscriber is: a use case should not have to
/// be handed a metrics sink to be measured.
pub trait UseCaseObserver: Send + Sync + 'static {
    /// `use_case` is the span name (`notes.archive`), `outcome` `"ok"` or the
    /// [`DomainError::kind`] it failed with.
    fn finished(&self, use_case: &'static str, outcome: &'static str, elapsed: Duration);
}

static OBSERVER: OnceLock<Box<dyn UseCaseObserver>> = OnceLock::new();

/// Install the process-wide [`UseCaseObserver`]. Returns `false`, and leaves the first
/// one in place, if one was already installed.
pub fn observe_use_cases(observer: impl UseCaseObserver) -> bool {
    OBSERVER.set(Box::new(observer)).is_ok()
}

/// Labels a failed use case for its span and metrics.
///
/// Implemented for [`DomainError`]. A use case returning a narrower error type of its
/// own implements this to be usable under `#[use_cases]`.
pub trait UseCaseError {
    /// A stable, low-cardinality label such as `"not_found"`.
    fn kind(&self) -> &'static str;
}

impl UseCaseError for DomainError {
    fn kind(&self) -> &'static str {
        DomainError::kind(self)
    }
}

/// What `#[use_cases]` expands to. Not a stable API.
#[doc(hidden)]
pub mod __private {
    use super::*;

    /// Run one use case body inside its span, then report how it went.
    pub async fn observe<F, T, E>(
        name: &'static str,
        request_id: Option<Uuid>,
        body: F,
    ) -> Result<T, E>
    where
        F: Future<Output = Result<T, E>>,
        E: UseCaseError,
    {
        let span = match request_id {
            Some(id) => tracing::info_span!(
                "use_case",
                otel.name = name,
                use_case = name,
                request_id = %id,
                outcome = tracing::field::Empty,
            ),
            None => tracing::info_span!(
                "use_case",
                otel.name = name,
                use_case = name,
                outcome = tracing::field::Empty,
            ),
        };
        let started = Instant::now();
        let result = body.instrument(span.clone()).await;
        let outcome = match &result {
            Ok(_) => "ok",
            Err(e) => e.kind(),
        };
        span.record("outcome", outcome);
        if let Some(observer) = OBSERVER.get() {
            observer.finished(name, outcome, started.elapsed());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Seen(Mutex<Vec<(&'static str, &'static str)>>);

    static SEEN: OnceLock<std::sync::Arc<Seen>> = OnceLock::new();

    struct Forward(std::sync::Arc<Seen>);

    impl UseCaseObserver for Forward {
        fn finished(&self, use_case: &'static str, outcome: &'static str, _: Duration) {
            self.0.0.lock().unwrap().push((use_case, outcome));
        }
    }

    fn seen() -> &'static std::sync::Arc<Seen> {
        SEEN.get_or_init(|| {
            let seen = std::sync::Arc::new(Seen::default());
            observe_use_cases(Forward(seen.clone()));
            seen
        })
    }

    #[tokio::test]
    async fn the_observer_hears_every_outcome() {
        let seen = seen();

        let ok: Result<u8, DomainError> =
            __private::observe("test.ok", Some(Uuid::new_v4()), async { Ok(1) }).await;
        assert_eq!(ok.unwrap(), 1);

        let failed: Result<(), DomainError> = __private::observe("test.missing", None, async {
            Err(DomainError::not_found())
        })
        .await;
        assert!(failed.is_err());

        let recorded = seen.0.lock().unwrap().clone();
        assert!(recorded.contains(&("test.ok", "ok")), "{recorded:?}");
        assert!(
            recorded.contains(&("test.missing", "not_found")),
            "{recorded:?}"
        );
    }

    #[test]
    fn only_the_first_observer_is_installed() {
        seen();
        assert!(!observe_use_cases(Forward(std::sync::Arc::default())));
    }
}
