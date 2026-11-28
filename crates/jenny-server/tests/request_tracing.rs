//! Which requests are traced (ADR-020): probes are not unless asked, the API and its
//! 404s are, and every response carries a request id either way.
//!
//! A binary of its own, because it installs the process's global subscriber: a
//! thread-local one races with every other test hitting the same callsites, which
//! caches them as uninteresting.

use std::sync::{Arc, Mutex};

use jenny_rest::{RestAdapter, Routes};
use jenny_server::{ConfigSource, JennyServer, ServerArgs};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::layer::SubscriberExt as _;

/// Every span opened and every event emitted, by name and target.
#[derive(Clone, Default)]
struct Recorded(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorded {
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let name = attributes.metadata().name();
        self.0.lock().unwrap().push(format!("span {name}"));
    }

    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let target = event.metadata().target();
        self.0.lock().unwrap().push(format!("event {target}"));
    }
}

impl Recorded {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn traced(seen: &[String]) -> bool {
    seen.iter().any(|s| s == "span request") && seen.iter().any(|s| s.contains("tower_http"))
}

fn untraced(seen: &[String]) -> bool {
    !seen
        .iter()
        .any(|s| s == "span request" || s.contains("tower_http"))
}

async fn server(log_probes: bool) -> jenny_server::ServerParts {
    let args = ServerArgs {
        address: Some("127.0.0.1:0".into()),
        env_prefix: Some("JENNY_TEST_REQUEST_TRACING".into()),
        ..Default::default()
    };
    let source = ConfigSource::load(&args).unwrap();
    let mut config = source.server_config(&args).unwrap();
    config.server.endpoints.probes.log_requests = log_probes;
    JennyServer::from_config(config, source, jenny_core::Service::new("traced", "1.0.0"))
        .unwrap()
        .register(
            RestAdapter::new("my-notes")
                .unwrap()
                .mount(Routes::new("notes").get("/notes", || async { "[]" }))
                .unwrap(),
        )
        .unwrap()
        .bind()
        .await
        .unwrap()
}

/// Status line and headers, lowercased, of a plain HTTP/1.0 GET; the request's spans
/// and log lines are all recorded by the time the response is complete.
async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    // The access-log line is written as the response finishes; give it a moment.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    response.to_lowercase()
}

#[tokio::test]
async fn probes_are_not_traced_unless_asked_but_still_carry_a_request_id() {
    let recorded = Recorded::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(recorded.clone()))
        .unwrap();

    let parts = server(false).await;
    let addr = parts.local_addr().unwrap();
    let handle = parts.handle();
    let serving = tokio::spawn(parts.serve());
    recorded.take();

    let response = get(addr, "/j/health/ready").await;
    assert!(response.starts_with("http/1.0 200"), "{response}");
    assert!(
        response.contains(jenny_rest::REQUEST_ID_HEADER),
        "{response}"
    );
    let seen = recorded.take();
    assert!(
        untraced(&seen),
        "a probe left no span, no access log: {seen:?}"
    );

    let response = get(addr, "/api/notes").await;
    assert!(response.starts_with("http/1.0 200"), "{response}");
    let seen = recorded.take();
    assert!(traced(&seen), "the API is traced: {seen:?}");

    let response = get(addr, "/nowhere").await;
    assert!(response.starts_with("http/1.0 404"), "{response}");
    let seen = recorded.take();
    assert!(traced(&seen), "and so is a 404: {seen:?}");

    handle.shutdown();
    serving.await.unwrap().unwrap();

    let parts = server(true).await;
    let addr = parts.local_addr().unwrap();
    let handle = parts.handle();
    let serving = tokio::spawn(parts.serve());
    recorded.take();
    let response = get(addr, "/j/health/ready").await;
    assert!(
        response.contains(jenny_rest::REQUEST_ID_HEADER),
        "{response}"
    );
    let seen = recorded.take();
    assert!(traced(&seen), "log_requests: true: {seen:?}");

    handle.shutdown();
    serving.await.unwrap().unwrap();
}
