//! End-to-end test: boots the real service and drives it over HTTP.
//!
//! It goes through [`build`](crate::build) — the same wiring `main` uses — so what is
//! exercised is the actual composition root, not a rebuilt approximation of it. And it
//! binds port `0` and asks the listener what it got, so the test never races another
//! process for a fixed port.

use jenny::server::{JennyServer, ServerArgs};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

struct Service {
    client: Client,
    base: String,
}

impl Service {
    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.read(self.client.get(self.url(path))).await
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.read(self.client.post(self.url(path)).json(&body))
            .await
    }

    async fn post_empty(&self, path: &str) -> StatusCode {
        self.client
            .post(self.url(path))
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn put(&self, path: &str, body: Value) -> StatusCode {
        self.client
            .put(self.url(path))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn delete(&self, path: &str) -> (StatusCode, Value) {
        self.read(self.client.delete(self.url(path))).await
    }

    /// A JSON Patch, optionally conditional, returning the status and the body.
    async fn patch(
        &self,
        path: &str,
        operations: Value,
        if_match: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .client
            .patch(self.url(path))
            .header("content-type", "application/json-patch+json")
            .body(operations.to_string());
        if let Some(tag) = if_match {
            request = request.header("if-match", tag);
        }
        self.read(request).await
    }

    /// The entity tag a GET reports.
    async fn etag(&self, path: &str) -> String {
        let response = self.client.get(self.url(path)).send().await.unwrap();
        response.headers()["etag"].to_str().unwrap().to_string()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn read(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body)
    }
}

async fn health_endpoints_describe_the_service(svc: &Service) {
    let (status, live) = svc.get("/j/health/live").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(live, json!({ "status": "up" }));

    let (status, ready) = svc.get("/j/health/ready").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ready["status"], "up");
    // Notes are in memory here; with a backend configured, `postgres`, `nats` or
    // `objects` is listed.
    assert!(
        ready["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| check["status"] == "up")
    );

    let (status, info) = svc.get("/j/health/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["service"], "basic-example");
    assert_eq!(info["service_version"], env!("CARGO_PKG_VERSION"));
    assert!(
        info["framework_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert_eq!(info["adapters"][0]["name"], "my-notes");
    assert_eq!(info["adapters"][0]["path"], "/api");
    assert_eq!(info["adapters"][0]["resources"][0], "urn:my-notes:notes");
}

async fn a_note_can_be_created_read_updated_and_deleted(svc: &Service) {
    let (status, id) = svc
        .post("/api/notes", json!({ "title": "Shopping", "body": "milk" }))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let id: Uuid = serde_json::from_value(id).unwrap();

    let (status, note) = svc.get(&format!("/api/notes/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(note["title"], "Shopping");
    assert_eq!(note["body"], "milk");
    assert_eq!(note["archived"], false);

    let (status, all) = svc.get("/api/notes").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all.as_array().unwrap().len(), 1);

    let status = svc
        .put(
            &format!("/api/notes/{id}"),
            json!({ "title": "Groceries", "body": "eggs" }),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, updated) = svc.get(&format!("/api/notes/{id}")).await;
    assert_eq!(updated["title"], "Groceries");

    let (status, removed) = svc.delete(&format!("/api/notes/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(removed["title"], "Groceries");

    let (status, _) = svc.get(&format!("/api/notes/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, all) = svc.get("/api/notes").await;
    assert!(all.as_array().unwrap().is_empty());
}

async fn listings_can_be_sorted_filtered_and_paged(svc: &Service) {
    for title in ["Alpha", "Beta", "Gamma", "Delta"] {
        svc.post("/api/notes", json!({ "title": title, "body": "x" }))
            .await;
    }

    let titles = |body: &Value| -> Vec<String> {
        body.as_array()
            .unwrap()
            .iter()
            .map(|n| n["title"].as_str().unwrap().to_string())
            .collect()
    };

    let (status, body) = svc.get("/api/notes?sort=title").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&body), ["Alpha", "Beta", "Delta", "Gamma"]);

    let (_, body) = svc.get("/api/notes?sort=-title").await;
    assert_eq!(titles(&body), ["Gamma", "Delta", "Beta", "Alpha"]);

    let (_, body) = svc.get("/api/notes?filter=title:begins:a").await;
    assert_eq!(titles(&body), ["Alpha"]);

    let (_, body) = svc
        .get("/api/notes?filter=title:contains:a&sort=title")
        .await;
    assert_eq!(titles(&body), ["Alpha", "Beta", "Delta", "Gamma"]);

    let (_, body) = svc.get("/api/notes?sort=title&page=2&page_size=2").await;
    assert_eq!(titles(&body), ["Delta", "Gamma"]);

    // A path outside the allowlist is refused rather than reaching storage.
    let (status, body) = svc.get("/api/notes?sort=password_hash").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["detail"].as_str().unwrap().contains("not queryable"),
        "got {}",
        body["detail"]
    );

    // So is a malformed one.
    let (status, _) = svc.get("/api/notes?filter=title:nope:x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    for title in titles(&svc.get("/api/notes").await.1) {
        let (_, all) = svc.get("/api/notes").await;
        let id = all
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["title"] == title)
            .map(|n| n["id"].as_str().unwrap().to_string())
            .unwrap();
        svc.delete(&format!("/api/notes/{id}")).await;
    }
}

async fn a_domain_rule_becomes_the_right_status(svc: &Service) {
    let (_, id) = svc
        .post("/api/notes", json!({ "title": "Archive me", "body": "" }))
        .await;
    let id: Uuid = serde_json::from_value(id).unwrap();

    assert_eq!(
        svc.post_empty(&format!("/api/notes/{id}/archive")).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        svc.post_empty(&format!("/api/notes/{id}/archive")).await,
        StatusCode::CONFLICT,
        "archiving twice is a conflict the domain raised, mapped to 409"
    );

    // An archived note is immutable — again, a domain rule, surfaced as 409.
    let status = svc
        .put(&format!("/api/notes/{id}"), json!({ "title": "Nope" }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    svc.delete(&format!("/api/notes/{id}")).await;
}

async fn a_note_can_carry_an_attachment(svc: &Service) {
    let (_, id) = svc.post("/api/notes", json!({ "title": "Receipt" })).await;
    let id: Uuid = serde_json::from_value(id).unwrap();
    let attachment = svc.url(&format!("/api/notes/{id}/attachment"));

    let response = svc
        .client
        .put(&attachment)
        .header("content-type", "image/png")
        .body(&b"\x89PNG not really"[..])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = svc.client.get(&attachment).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(&response.bytes().await.unwrap()[..], b"\x89PNG not really");

    // A file has a GET and a PUT, but no JSON to patch.
    let (status, _) = svc
        .patch(
            &format!("/api/notes/{id}/attachment"),
            json!([{ "op": "remove", "path": "/x" }]),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Deleting the note deletes its file.
    let (status, _) = svc.delete(&format!("/api/notes/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let response = svc.client.get(&attachment).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

async fn notes_can_be_patched_and_writes_can_be_made_conditional(svc: &Service) {
    let (_, id) = svc
        .post("/api/notes", json!({ "title": "Shopping", "body": "milk" }))
        .await;
    let id: Uuid = serde_json::from_value(id).unwrap();
    let note = format!("/api/notes/{id}");
    assert_eq!(svc.etag(&note).await, r#""1""#);

    // A patch derived from GET and PUT: no PATCH handler exists in the adapter.
    let replace_body = json!([{ "op": "replace", "path": "/body", "value": "eggs" }]);
    let (status, _) = svc.patch(&note, replace_body.clone(), Some(r#""1""#)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, patched) = svc.get(&note).await;
    assert_eq!(patched["title"], "Shopping");
    assert_eq!(patched["body"], "eggs");
    assert_eq!(svc.etag(&note).await, r#""2""#);

    // The same precondition again: the note moved on, so nothing is overwritten.
    let (status, problem) = svc.patch(&note, replace_body, Some(r#""1""#)).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(problem["type"], "urn:jenny:problem:precondition-failed");

    // `archived` is in what GET returns but not in what PUT takes: refused, not dropped.
    let (status, problem) = svc
        .patch(
            &note,
            json!([{ "op": "replace", "path": "/archived", "value": true }]),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(problem["detail"].as_str().unwrap().contains("/archived"));

    // A plain JSON body is not a patch.
    let status = svc
        .client
        .patch(svc.url(&note))
        .json(&json!({ "body": "x" }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // PUT and DELETE honour If-Match too.
    let put = |tag: &'static str| {
        svc.client
            .put(svc.url(&note))
            .header("if-match", tag)
            .json(&json!({ "title": "Groceries", "body": "eggs" }))
            .send()
    };
    assert_eq!(
        put(r#""1""#).await.unwrap().status(),
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        put(r#""2""#).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let delete = |tag: &'static str| {
        svc.client
            .delete(svc.url(&note))
            .header("if-match", tag)
            .send()
    };
    assert_eq!(
        delete(r#""2""#).await.unwrap().status(),
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(delete(r#""3""#).await.unwrap().status(), StatusCode::OK);

    // The collection has no PUT, so it has no derived PATCH either.
    let (status, _) = svc.patch("/api/notes", json!([]), None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

async fn failures_are_problem_documents(svc: &Service) {
    // A value object refused to be constructed -> 400 with the reason.
    let (status, body) = svc.post("/api/notes", json!({ "title": "   " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "urn:jenny:problem:invalid");
    assert_eq!(body["status"], 400);
    // The message is field-scoped because the `Dto` derive reports it that way.
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("title: must not be empty"),
        "got {}",
        body["detail"]
    );

    // A malformed body gets the same shape rather than a bare string.
    let malformed = svc
        .client
        .post(svc.url("/api/notes"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        malformed.headers().get("content-type").unwrap(),
        "application/problem+json"
    );

    // So does a route that does not exist...
    let (status, body) = svc.get("/api/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["type"], "urn:jenny:problem:not-found");

    // ...and a method that is not allowed on one that does.
    let status = svc.put("/api/notes", json!({ "title": "x" })).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn the_example_service_works_end_to_end() {
    let args = ServerArgs {
        // Port 0: let the OS pick, then ask what it picked.
        address: Some("127.0.0.1:0".to_string()),
        config_path: Some(format!(
            "{}/../../examples/basic/config.yaml",
            env!("CARGO_MANIFEST_DIR")
        )),
        ..Default::default()
    };

    let parts = crate::build(JennyServer::from_parsed_args(args).unwrap())
        .unwrap()
        .bind()
        .await
        .unwrap();

    let svc = Service {
        client: Client::new(),
        base: format!("http://{}", parts.local_addr().unwrap()),
    };
    let handle = parts.handle();
    let serving = tokio::spawn(parts.serve());

    health_endpoints_describe_the_service(&svc).await;
    a_note_can_be_created_read_updated_and_deleted(&svc).await;
    listings_can_be_sorted_filtered_and_paged(&svc).await;
    a_domain_rule_becomes_the_right_status(&svc).await;
    notes_can_be_patched_and_writes_can_be_made_conditional(&svc).await;
    a_note_can_carry_an_attachment(&svc).await;
    failures_are_problem_documents(&svc).await;

    handle.shutdown();
    serving.await.unwrap().unwrap();
}
