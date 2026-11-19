# ADR-018: Object Storage

**Status:** Accepted — implemented; conditional writes jenny provides itself, independent of the backend, not implemented
**Date:** 2026-11-18, revised 2026-11-19

## Context

ADR-013 decided that object storage gets a port (`Blobs`), an adapter over the Apache
Arrow project's `object_store`, and the S3 API, rather than MinIO, as the
integration. What that left open:

- **What the port promises.** `object_store` exposes much more than a service needs:
  multipart uploads, ranged reads, copies, tags, and per-backend extensions. A port that
  mirrors it would make the in-memory implementation a second object store.
- **Conditional writes.** S3 has had `If-None-Match` and `If-Match` on `PutObject` since
  2024, and most S3-compatible servers have followed. `object_store` sends them by
  default, so a server without them ignores the headers and overwrites without anyone
  noticing.
- **Configuration and credentials.** "It only needs config" has to hold on AWS, where
  credentials come from a role and not from a file, as much as against a local server.
- **What a test runs against.** The MinIO image this would once have meant is gone
  from Docker Hub.

## Decision

### A small port, whole blobs

```rust,ignore
#[async_trait]
pub trait Blobs: Send + Sync + 'static {
    async fn put(&self, key: &str, blob: Blob) -> DomainResult<BlobInfo>;
    async fn put_if_absent(&self, key: &str, blob: Blob) -> DomainResult<BlobInfo>;
    async fn put_if_match(&self, key: &str, blob: Blob, etag: &str) -> DomainResult<BlobInfo>;
    async fn get(&self, key: &str) -> DomainResult<Option<(Blob, BlobInfo)>>;
    async fn head(&self, key: &str) -> DomainResult<Option<BlobInfo>>;
    async fn delete(&self, key: &str) -> DomainResult<()>;
    async fn list(&self, prefix: &str) -> DomainResult<Vec<BlobInfo>>;
    async fn signed_url(&self, key: &str, access: Access, valid_for: Duration)
        -> DomainResult<String>;
}
```

A `Blob` is bytes and an optional media type. Blobs are read and written whole: what a
use case attaches, exports or renders fits in memory, and streaming would put a stream
type from some runtime crate into the port. For anything larger, the answer is
`signed_url`: the client uploads or downloads directly from the store, and the bytes
never pass through the service. What does pass through is bounded by
`server.body_limit` (256 KiB by default), like every request body.

The failures are domain failures, so the HTTP layer already knows them:
`put_if_absent` on an existing key is `Conflict` (409), and `put_if_match` against a
changed blob is `Stale` (412), just as a versioned `Store` save is. So is
`put_if_match` on a key that is gone: HTTP's `If-Match` fails on a missing resource
too, and `object_store` reports S3's 404 for it as a failed precondition. `delete` of a
missing key succeeds, so a retried delete is not an error.

Keys are paths, checked the same way by every implementation (`check_key`): `/`
separated segments, none empty, `.` or `..`. `list("avatars")` lists under the path
`avatars/`, not every key that starts with `avatars`, because `object_store` lists by
path and an in-memory store that matched characters would disagree with it.

`MemoryBlobs` in `jenny_core::memory` implements the port for tests and for services
that start without a store. It keeps the same contract, minus signed URLs.

### One adapter crate, configured by URL

`jenny-objects` holds `ObjectBlobs` and `ObjectsConfig`. It is a crate of its own,
unlike the NATS store in ADR-017, because nothing else in the workspace uses
`object_store`, and a service that wants Postgres should not compile an S3 client.

The `objects` section is a URL and a map of `object_store`'s own option names:

```yaml
objects:
  url: s3://attachments/notes      # bucket, then an optional key prefix
  options:
    endpoint: http://localhost:9000
```

`s3://` builds an S3 client from `AmazonS3Builder::from_env()` with the options laid
over it. The standard `AWS_*` variables therefore apply, and on AWS the URL alone is
enough: the role's credentials (IRSA, ECS, instance profile) are found the way every
AWS SDK finds them. `file:///dir` and `memory://` are for development and tests.

The options use `object_store`'s names, not a jenny vocabulary. A translation would be
one more thing to document and one more place to fall behind the crate. The option map
can carry a secret, so its `Debug` prints the keys only.

The prefix in the URL lets several services, or several environments, share a bucket
without seeing each other's keys. `list` returns keys without it.

### Conditional writes are off over S3 until a service turns them on

`put_if_absent` is `PutMode::Create`, and `put_if_match` is `PutMode::Update`. On S3
they are `If-None-Match: *` and `If-Match` headers. AWS S3 honours them, and so do the
servers this is tested against. A server that ignores them overwrites anyway, and the
client cannot tell. Writing unconditionally is the one failure a caller who asked for
a condition cannot detect.

`object_store` 0.14 sends the headers by default (`conditional_put: etag`). jenny does
not: `ObjectBlobs::from_url` sets `conditional_put: disabled` unless
`objects.options.conditional_put` or `AWS_CONDITIONAL_PUT` names a value, so the
service decides, not the library. Until then the conditional writes are refused with
an internal error that names the setting, before any request is sent. Plain `put` is
unaffected. A service whose server honours the headers sets `conditional_put: etag`.
Local files do `put_if_absent` only, and memory does both.

The first version of this decision kept `object_store`'s default, on the grounds that
an opt-in adds a setting every AWS deployment must remember. It was reversed. On a
server that ignores the headers that default guards against nothing, and the opt-in
costs one setting, paid only by the services that use conditional writes. The example
and the scaffold use none. An earlier draft also had `object_store`'s own default
backwards, read off the documentation of older releases.

`MemoryBlobs` does conditional writes without being asked, as a server that honours
them would. A service that relies on them in its tests needs `conditional_put: etag`
wherever it runs against S3, or its first conditional write there is a 500. Making the
in-memory store refuse by default as well, for the same answer everywhere, belongs with
conditional writes jenny provides itself (below).

### Readiness lists the bucket

`JennyServer::objects()` builds the store on first call and registers an `objects`
readiness check. The check lists under a path nothing is stored at. A `HEAD` would have
been the obvious probe, but S3 answers a `HEAD` in a missing bucket with the same 404 it
uses for a missing key, so a store whose bucket does not exist would report ready. A
test pins this.

The runtime does not create buckets. A bucket is infrastructure, with policies,
lifecycle rules and encryption settings that the service should not decide. The
scaffold's `compose.yaml` and `just test-objects` create one by hand.

### Tested against a server that checks signatures

`just test-objects` and CI run RustFS 1.0.6, which is Apache-2.0 licensed, S3
compatible, supports `If-None-Match` and `If-Match`, and verifies signatures. The last
point is why it was chosen over SeaweedFS, which passes the same conditional-write
probes but accepts unsigned requests by default. With RustFS, the signed-URL test
shows that the signature is what let the request in: the same URL without it is
refused. The tests read `JENNY_TEST_S3_URL` and the `AWS_*` variables, the same way a
deployed service does, and skip without them, or fail under `CI`.

### In the example

A note can carry one attachment: `PUT` and `GET /notes/{id}/attachment`, kept in
`Blobs` under `notes/{id}/attachment`. Deleting the note deletes the attachment second,
so a failure leaves an orphaned blob, never a note that points at nothing. The rule that
an archived note takes no attachment lives on `Note`.

An attachment has a `GET` and a `PUT`, so in JSON Patch mode it gets a derived `PATCH`.
It used to answer that with a 500. It now answers 415 without `Accept-Patch`, because a
resource that is not JSON takes no patch format (RFC 5789 §2.2).

## Consequences

- A service gets file storage from one URL, and swaps S3 for a directory or memory
  without touching a use case.
- Large files are handled by URL, not by the port. A service that must stream through
  itself uses `object_store` directly, in an adapter of its own.
- Conditional writes over S3 are only as good as the server's support for them, so a
  service turns them on with `conditional_put: etag` once it trusts its server. Until
  then they are an error, never a silent overwrite.
- Blobs and aggregates are not written atomically. A use case that writes both orders
  the writes so that a failure leaves garbage rather than a dangling reference, as the
  example does.

## Not built yet

- Azure Blob Storage and Google Cloud Storage. `object_store` supports both, behind
  features this crate does not enable. Each is a URL scheme and a feature away.
- Streaming reads and writes, and multipart uploads, for blobs that do not fit in
  memory.
- Signed URLs with conditions (content type, maximum size) for browser uploads.
- Conditional writes jenny provides itself, so that `put_if_absent` and `put_if_match`
  mean the same on every backend, and a service opts into them instead of inheriting
  whatever its server supports. Like `ETag` and `If-Match` for aggregates (ADR-014), it
  is an opt-in mechanism that is the same whatever the storage. One design that holds
  across replicas keeps each blob's current version in a versioned `Store`, whose save
  is a compare-and-swap on every backend (ADR-012). The bytes are written under a fresh
  key, and the record is swapped to point at them. The entity tag is then a `Version`,
  so `Versioned` and `IfMatch` serve blobs as they serve aggregates. The open question is
  signed write URLs, whose uploads would bypass the record.
