//! A complete jenny service, laid out the way jenny expects.
//!
//! ```text
//! domain/          the hexagon — entities, value objects, events, the ports it owns
//! application/     use cases — orchestration, and nothing else
//! adapter/
//!   inbound/rest   HTTP in: DTOs, TryFrom, status codes
//!   outbound/      the driven side: an event publisher and the links between notes
//!                  (the note store is jenny's)
//! main.rs          the composition root — the only file that knows every layer
//! ```
//!
//! Run it:
//!
//! ```sh
//! cargo run --example basic --features server -- --config-path examples/basic/config.yaml
//! ```
//!
//! That keeps the notes in memory. With the `persistence` feature and a database URL
//! they live in Postgres instead, one `jsonb` row each, the table created at startup —
//! or, with `messaging` and `JENNY__MESSAGING__NATS__URL`, in a NATS key-value bucket:
//!
//! ```sh
//! docker run -d --rm -p 5432:5432 -e POSTGRES_PASSWORD=notes postgres:18
//! JENNY__PERSISTENCE__POSTGRES__URL=postgres://postgres:notes@localhost/postgres \
//!   cargo run --example basic --features server,persistence -- \
//!   --config-path examples/basic/config.yaml
//!
//! docker run -d --rm -p 4222:4222 nats:2-alpine -js
//! JENNY__MESSAGING__NATS__URL=nats://localhost:4222 \
//!   cargo run --example basic --features server,messaging -- \
//!   --config-path examples/basic/config.yaml
//! ```
//!
//! Attachments stay in memory too, unless the `objects` feature and an `objects.url` put
//! them elsewhere — a directory is enough to try it, an S3 bucket in production:
//!
//! ```sh
//! JENNY__OBJECTS__URL=file:///tmp/notes \
//!   cargo run --example basic --features server,objects -- \
//!   --config-path examples/basic/config.yaml
//! ```
//!
//! Links between notes are kept in memory as well, or, with the `graph` feature and a
//! database that has Apache AGE, in a graph beside the notes:
//!
//! ```sh
//! docker run -d --rm -p 5432:5432 -e POSTGRES_PASSWORD=notes apache/age:release_PG18_1.8.0
//! JENNY__PERSISTENCE__POSTGRES__URL=postgres://postgres:notes@localhost/postgres \
//!   cargo run --example basic --features server,graph -- \
//!   --config-path examples/basic/config.yaml
//! ```
//!
//! Then poke at it:
//!
//! ```sh
//! curl localhost:8000/j/health/ready
//!
//! ID=$(curl -sX POST localhost:8000/api/notes \
//!   -H 'content-type: application/json' \
//!   -d '{"title": "Shopping", "body": "milk"}' | tr -d '"')
//!
//! curl localhost:8000/api/notes
//! curl -X PUT localhost:8000/api/notes/$ID/attachment -H 'content-type: text/plain' \
//!   --data-binary 'milk, eggs'                        # 204; GET returns the file
//! curl -X PUT localhost:8000/api/notes/$ID/links/$OTHER  # 204, given a second note
//! curl localhost:8000/api/notes/$ID/links             # {"to": [OTHER], "from": []}
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

use jenny::domain::ValueKind;
use jenny::memory::{Memory, MemoryBlobs};
use jenny::rest::RestAdapter;
use jenny::server::JennyServer;
use jenny::{EventPublisher, Events};
use serde::Deserialize;

#[cfg(not(feature = "messaging"))]
use adapter::outbound::events::EventLog;
use adapter::outbound::links::MemoryLinks;
use application::Notes;
use domain::model::Note;
use domain::port::{Attachments, NoteLinks, NoteStore};

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

/// What a note listing may sort and filter on, and what each attribute holds.
///
/// The security boundary for client-supplied criteria: a listing that names anything
/// else is refused before it reaches the data, so `filter=…` cannot become a question
/// about a field the API never shows. Every store takes the same list.
const QUERYABLE: [(&str, ValueKind); 3] = [
    ("title", ValueKind::Text),
    ("body", ValueKind::Text),
    ("archived", ValueKind::Bool),
];

/// Where notes live, from configuration: Postgres when `persistence.postgres.url` is set
/// (with the `persistence` feature), else a NATS key-value bucket when
/// `messaging.nats.url` is (with `messaging`), else memory.
///
/// One line each, because a store needs nothing but the aggregate's serde derives and
/// every store takes the same allowlist; the runtime prepares the table or the bucket
/// and reports its readiness.
fn note_store(server: &mut JennyServer) -> anyhow::Result<Arc<NoteStore>> {
    #[cfg(feature = "persistence")]
    if let Some(postgres) = server.postgres()? {
        return Ok(Arc::new(postgres.documents::<Note>().queryable(QUERYABLE)));
    }
    #[cfg(feature = "messaging")]
    if let Some(nats) = server.nats() {
        return Ok(Arc::new(nats.kv::<Note>().queryable(QUERYABLE)));
    }
    let _ = server;
    Ok(Arc::new(Memory::<Note>::new().queryable(QUERYABLE)))
}

/// Where attachments live: an object store when `objects.url` is set (with the
/// `objects` feature) — an S3 bucket, a directory — else memory.
fn attachments(server: &mut JennyServer) -> anyhow::Result<Arc<Attachments>> {
    #[cfg(feature = "objects")]
    if let Some(objects) = server.objects()? {
        return Ok(objects);
    }
    let _ = server;
    Ok(Arc::new(MemoryBlobs::new()))
}

/// Where links between notes live: an Apache AGE graph named `notes`, in the same
/// database as the notes, with the `graph` feature and `persistence.postgres.url` set,
/// else memory. The runtime creates the graph before serving.
fn note_links(server: &mut JennyServer) -> anyhow::Result<Arc<dyn NoteLinks>> {
    #[cfg(feature = "graph")]
    if let Some(postgres) = server.postgres()? {
        let graph = postgres.graph("notes");
        return Ok(Arc::new(adapter::outbound::links::GraphLinks::new(graph)));
    }
    let _ = server;
    Ok(Arc::new(MemoryLinks::default()))
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
    let store = note_store(&mut server)?;
    let attachments = attachments(&mut server)?;
    let links = note_links(&mut server)?;

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
    let notes = Notes::new(store, attachments, links, Events::new(events));

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

    server
        .with_version(jenny::service_version!())
        .register(rest)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    build(JennyServer::from_args()?)?.run().await
}
