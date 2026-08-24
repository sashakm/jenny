//! The hexagon. Depends on nothing but `jenny::domain` and `jenny::port`.
//!
//! Look at the imports in the files below: there is no `serde`, no `sqlx`, no `axum`.
//! That is the dependency rule made visible. If a `use` in here ever needed one of
//! those, something has been put in the wrong ring.

pub mod event;
pub mod model;
pub mod port;
