//! Driving adapters — the outside world calling in.
//!
//! One module per protocol. Each owns its own DTOs and its own status mapping, and
//! each converges on the same commands from `application/command.rs`. A gRPC adapter
//! would sit beside `rest` here with its own protobuf types and the same use-case
//! calls underneath.

#[cfg(feature = "messaging")]
pub mod events;
pub mod rest;
pub mod tasks;
