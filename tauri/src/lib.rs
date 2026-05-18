//! hypehouse-desktop library surface.
//!
//! Exists so the integration tests in `tests/` can import `sidecar` +
//! `commands` without duplicating the source via `#[path = "..."]`.
//! The binary entrypoint in `src/main.rs` simply uses the same
//! modules through `crate::`.

pub mod commands;
pub mod sidecar;
