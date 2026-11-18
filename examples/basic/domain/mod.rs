//! The hexagon. Depends on nothing but `jenny::domain` and `jenny::port`.
//!
//! Look at the imports in the files below: there is no `sqlx`, no `axum`, no broker.
//! That is the dependency rule made visible. If a `use` in here ever needed one of
//! those, something has been put in the wrong ring. `serde` is the exception, and a
//! deliberate one: it is what lets a store persist the aggregate and a publisher send
//! the events without a mapping layer for each.

pub mod event;
pub mod model;
pub mod port;
