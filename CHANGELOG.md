# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0](https://github.com/sashakm/jenny/releases/tag/v0.1.0) - 2026-11-25

### 🚀 Features

- Agent files and cli
- [**breaking**] Replace RBAC with Cedar and fix the review findings
- [**breaking**] Versioned stores, serde aggregates and observed use cases
- [**breaking**] JSON Patch mode and conditional requests
- [**breaking**] Postgres document store and a runtime-managed pool
- Postgres table store over a table of your own
- [**breaking**] Raise events with the write, and a Postgres outbox
- NATS JetStream key-value store
- Object storage behind a Blobs port, over S3 and compatibles
- Apache AGE graphs in the same Postgres, behind a `graph` feature

### 🐛 Bug Fixes

- *(objects)* Say that S3 conditional writes are on by default, and make a missing key stale everywhere
- *(objects)* Conditional writes over S3 are off until a service turns them on

### 📚 Documentation

- Bring README, ADRs and rustdoc in line with stores and JSON Patch
- Bring README, ADRs and rustdoc in line with the Postgres stores and outbox
- Bring README, ADRs and examples in line with the NATS key-value store
- Bring the docs in line with object storage and AGE graphs
- *(adr)* Propose the day-2 roadmap as ADR-020 to ADR-030
- *(adr)* Propose NATS authentication and event envelope enrichment

### ⚙️ Miscellaneous Tasks

- Initial commit
