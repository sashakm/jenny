//! A complete jenny service, laid out the way jenny expects.
//!
//! ```text
//! domain/          the hexagon — entities, value objects, events, the ports it owns
//! application/     use cases — orchestration, and nothing else
//! adapter/
//!   inbound/rest   HTTP in: DTOs, TryFrom, status codes
//!   outbound/      the driven side: a note store, an event publisher
//! main.rs          the composition root — the only file that knows every layer
//! ```
//!
//! Run it:
//!
//! ```sh
//! cargo run --example basic --features server -- --config-path examples/basic/config.yaml
//! ```
//!
//! Then poke at it:
//!
//! ```sh
//! curl localhost:8000/health/ready
//!
//! ID=$(curl -sX POST localhost:8000/api/notes \
//!   -H 'content-type: application/json' \
//!   -d '{"title": "Shopping", "body": "milk"}' | tr -d '"')
//!
//! curl localhost:8000/api/notes
//! curl -X POST localhost:8000/api/notes/$ID/archive   # 204
//! curl -X POST localhost:8000/api/notes/$ID/archive   # 409, already archived
//! curl -X POST localhost:8000/api/notes -H 'content-type: application/json' \
//!   -d '{"title": "  "}'                              # 400, title must not be empty
//! ```

mod adapter;
mod application;
mod domain;

#[cfg(test)]
mod example_test;

use std::sync::Arc;

use async_trait::async_trait;
use jenny::EventPublisher;
use jenny::rest::RestAdapter;
use jenny::server::{HealthCheck, JennyServer};
use serde::Deserialize;

#[cfg(not(feature = "messaging"))]
use adapter::outbound::events::EventLog;
use adapter::outbound::memory::InMemoryNotes;
use application::Notes;

/// This service's own configuration, read from `applications.my-notes`. The framework
/// imposes no schema on it — the shape below is entirely ours.
#[derive(Debug, Deserialize)]
struct NotesConfig {
    greeting: String,
}

impl Default for NotesConfig {
    fn default() -> Self {
        Self {
            greeting: "hello from the built-in default".to_string(),
        }
    }
}

/// A readiness signal backed by the note store. `/health/ready` reports it, so a load
/// balancer stops sending traffic when the store is unusable — while `/health/live`
/// stays up, because restarting the process would not fix a broken dependency.
struct StoreIsUsable(Arc<InMemoryNotes>);

#[async_trait]
impl HealthCheck for StoreIsUsable {
    fn name(&self) -> &str {
        "note-store"
    }

    async fn check(&self) -> Result<(), String> {
        if self.0.is_healthy() {
            Ok(())
        } else {
            Err("note store is unavailable".to_string())
        }
    }
}

/// Wire the service together.
///
/// Read it top to bottom and you have the entire dependency graph: driven adapters
/// first, then the use cases that depend on them, then the driving adapter that
/// depends on those. Every arrow points inward. There is no container, and nothing is
/// resolved at runtime — if it is not here, it does not exist.
///
/// Shared by `main` and the integration test, so the test exercises the real wiring.
fn build(mut server: JennyServer) -> anyhow::Result<JennyServer> {
    let config: NotesConfig = server.context().app_section("my-notes").unwrap_or_default();
    tracing::info!("my-notes says: {}", config.greeting);

    // --- driven adapters: the outside world, as the domain's ports see it ----------
    let repository = Arc::new(InMemoryNotes::default());

    // Erased on purpose, to show the other half of the choice: the repository below is
    // a generic parameter (static dispatch, monomorphised), while the publisher is
    // picked at the composition root and passed as a trait object. Both are ports;
    // which dispatch you use is a wiring decision, not a design one.
    //
    // With the `messaging` feature the runtime has already started an in-process bus,
    // so the port comes from it and one publish fans out to every listener. Without
    // it, a publisher that writes to the log satisfies the same port.
    #[cfg(feature = "messaging")]
    let events: Arc<dyn EventPublisher> = {
        server.subscribe(adapter::outbound::events::EventLogListener)?;
        server.events()?
    };
    #[cfg(not(feature = "messaging"))]
    let events: Arc<dyn EventPublisher> = Arc::new(EventLog);

    // Anything several adapters need can be shared through the context while wiring.
    server.context_mut().provide(repository.clone());

    // --- application: use cases over those ports ----------------------------------
    let notes = Arc::new(Notes::new(repository.clone(), events));

    // --- driving adapter: HTTP in -------------------------------------------------
    // No mount path given, so it takes `server.api_path` from configuration, which
    // itself defaults to `/api`.
    let rest = RestAdapter::new("my-notes")?
        .describe("Note taking")
        .mount(adapter::inbound::rest::routes(notes))?;

    Ok(server
        .with_version(jenny::service_version!())
        .register(rest)?
        .register_health_check(StoreIsUsable(repository)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    build(JennyServer::from_args()?)?.run().await
}
