//! A complete jenny service, laid out the way jenny expects.
//!
//! ```text
//! domain/          the hexagon — entities, value objects, events, the ports it owns
//! application/     use cases — orchestration, and nothing else
//! adapter/
//!   inbound/rest   HTTP in: DTOs, TryFrom, status codes
//!   outbound/      the driven side: an event publisher (the note store is jenny's)
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
//! curl -X PATCH localhost:8000/api/notes/$ID -H 'if-match: "1"' \
//!   -H 'content-type: application/json-patch+json' \
//!   -d '[{"op": "replace", "path": "/body", "value": "eggs"}]'  # 204, 412 once it moved on
//! ```

mod adapter;
mod application;
mod domain;

#[cfg(test)]
mod example_test;

use std::sync::Arc;

use jenny::memory::Memory;
use jenny::rest::RestAdapter;
use jenny::server::JennyServer;
use jenny::{EventPublisher, Events};
use serde::Deserialize;

#[cfg(not(feature = "messaging"))]
use adapter::outbound::events::EventLog;
use application::Notes;
use domain::model::Note;

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
    //
    // The queryable paths are the security boundary for client-supplied criteria: a
    // listing that sorts or filters on anything else is refused before it reaches the
    // data, so `filter=…` cannot become a question about a field the API never shows.
    let store = Arc::new(Memory::<Note>::new().queryable(["title", "body", "archived"]));

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
    server.context_mut().provide(store.clone());

    // --- application: use cases over those ports ----------------------------------
    let notes = Notes::new(store.clone(), Events::new(events));

    // --- driving adapter: HTTP in -------------------------------------------------
    // No mount path given, so it takes `server.api_path` from configuration, which
    // itself defaults to `/api`.
    //
    // It accepts JSON Patch, so every route with a GET and a PUT also answers PATCH,
    // derived from those two. Drop that line and the adapter is plainly
    // representational: PUT replaces, PATCH is 405.
    let rest = RestAdapter::new("my-notes")?
        .describe("Note taking")
        .accept_json_patch()?
        .mount(adapter::inbound::rest::routes(notes))?;

    Ok(server
        .with_version(jenny::service_version!())
        .register(rest)?
        // `/health/ready` reports the store, so a load balancer stops sending traffic
        // when it is unusable — while `/health/live` stays up, because restarting the
        // process would not fix a broken dependency.
        .register_health_check(store))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    build(JennyServer::from_args()?)?.run().await
}
