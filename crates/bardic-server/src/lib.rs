//! Bardic server library. The binary in `main.rs` wires this up; integration
//! tests drive it in-process. See docs/ARCHITECTURE.md.

pub mod api;
pub mod app;
pub mod clock;
pub mod config;
pub mod error;
pub mod events;
pub mod lock;
pub mod store;

/// Version of `docs/contract/openapi.yaml` this server implements. A test
/// asserts it matches the contract file.
pub const API_VERSION: &str = "0.2.0";
