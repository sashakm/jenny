//! The infrastructure-free centre of a jenny service.
//!
//! `jenny-core` holds everything the inner rings of a hexagonal application are
//! allowed to depend on. Its own module tree encodes the ring the items belong to,
//! so the dependency rule is visible in every `use` statement:
//!
//! | Module | Ring | May be imported by |
//! |---|---|---|
//! | [`domain`] | the hexagon itself | `domain/`, `application/`, adapters, composition root |
//! | [`port`] | the boundary — ports are traits | `domain/`, `application/`, adapters, composition root |
//! | [`task`] | clock-driven inbound adapter | adapters, composition root |
//! | [`telemetry`] | outbound adapter (emit side) | composition root |
//!
//! Nothing in [`domain`] or [`port`] pulls in a database driver, an HTTP or gRPC
//! framework, or a message broker. That is what makes "the domain depends on
//! nothing external" checkable rather than aspirational: if a domain module needs
//! an import from outside `jenny_core::domain` / `jenny_core::port`, the dependency
//! rule has been broken.
//!
//! ### Dependencies point inward
//!
//! ```text
//!          ┌──────────── composition root (jenny-server) ────────────┐
//!          │  ┌───────── driving adapters ─────────┐                 │
//!          │  │   rest · messaging · task · cli    │                 │
//!          │  │        ┌──── application ────┐     │                 │
//!          │  │        │  ┌── domain ──┐     │     │                 │
//!          │  │        │  │  model     │     │     │                 │
//!          │  │        │  │  event     │     │     │                 │
//!          │  │        │  │  service   │     │     │                 │
//!          │  │        │  └────────────┘     │     │                 │
//!          │  │        │   use cases         │     │                 │
//!          │  │        └─────────────────────┘     │                 │
//!          │  └────────────────────────────────────┘                 │
//!          │     driven adapters: persistence · eventbus · security  │
//!          └─────────────────────────────────────────────────────────┘
//!               arrows of dependency all point toward the centre
//! ```

pub mod domain;
pub mod port;
pub mod task;
pub mod telemetry;

/// Your crate's version, for reporting as the service version.
///
/// A macro rather than a function because `env!` expands where it is *written*: called
/// from framework code it would yield the framework's version, which is not what
/// anyone means by "the service version". Expanded in your binary, it reads your
/// `Cargo.toml`.
///
/// ```
/// let version = jenny_core::service_version!();
/// assert!(!version.is_empty());
/// ```
#[macro_export]
macro_rules! service_version {
    () => {
        env!("CARGO_PKG_VERSION")
    };
}
