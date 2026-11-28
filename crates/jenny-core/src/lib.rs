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
//! | [`memory`] | in-memory store and blob store, driven adapters, and `Queryable`, the criteria evaluator | tests, composition root, other stores |
//! | [`testing`] | test doubles for the shipped ports | tests |
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
//!          │ driven adapters: stores · objects · eventbus · security │
//!          └─────────────────────────────────────────────────────────┘
//!               arrows of dependency all point toward the centre
//! ```

pub mod domain;
pub mod memory;
pub mod port;
pub mod task;
pub mod testing;

/// Who the service is: its name and version, as its own `Cargo.toml` states them.
///
/// Build it with [`service!`], which reads both at the call site. The runtime takes it
/// in `JennyServer::from_args(jenny::service!())`, and the top-level `service_name` and
/// `service_version` configuration keys override it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    /// The package name, such as `my-notes`.
    pub name: String,
    /// The package version, such as `1.2.3`.
    pub version: String,
}

impl Service {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// Your crate's name and version, as a [`Service`].
///
/// A macro for the same reason as [`service_version!`]: `env!` expands where it is
/// written, so called in your binary it reads your `Cargo.toml`, not jenny's.
///
/// ```
/// let service = jenny_core::service!();
/// assert_eq!(service.name, "jenny-core"); // expanded here, in jenny-core's own doc test
/// assert!(!service.version.is_empty());
/// ```
#[macro_export]
macro_rules! service {
    () => {
        $crate::Service::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
    };
}

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
