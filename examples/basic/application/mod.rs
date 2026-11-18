//! Use cases: orchestration, and nothing else.
//!
//! Each method here does the same few things — load, call the domain, save, publish
//! what the domain raised — and all of it is one call: `Store::change` loads the
//! aggregate, applies a domain method, saves it with a version check, and with
//! `.raising(…)` publishes what the method returned, in the same transaction when the
//! store keeps an outbox. The business
//! rules themselves are one ring in, in `domain/`; if a rule ever starts being written
//! *here*, it has leaked out of the hexagon.
//!
//! ### A struct, its ports, and a span per method
//!
//! [`Notes`] is a plain struct holding its outbound ports as trait objects. There is no
//! use-case trait: every driving adapter calls the methods directly, and the struct is
//! concrete, so nothing has to be erased to share it. `#[jenny::use_cases]` on the
//! `impl` block gives each method a span and a duration measurement and generates
//! nothing else — the dependency graph is still the struct's two fields.

pub mod command;
pub mod notes;

pub use notes::Notes;
