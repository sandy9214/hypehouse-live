//! Real Ableton Link integration — **placeholder, behind the
//! `ableton-link` Cargo feature flag (default off).**
//!
//! Per ADR-007 §v0.2, the full Link integration requires:
//!
//! 1. The official C++ Link SDK from Ableton (LGPL v3 — see ADR-009).
//! 2. A Rust binding (community `rust-link` crate, or a hand-rolled
//!    `bindgen` wrapper).
//! 3. ADR-009 sign-off on the LGPL licensing implications.
//!
//! All three are non-trivial and warrant their own PR. This file holds
//! a placeholder [`LinkReal`] type so the `cargo build --features
//! ableton-link` command-line still compiles, but every method panics
//! with `unimplemented!()` so we never silently ship a broken backend.
//!
//! Replacing this file is the entirety of the v0.2.x follow-up PR — the
//! `clock_sync::PeerClock` trait + `main.rs` wiring + ADR-009 sign-off
//! are all landed by this scaffold PR.

use super::PeerClock;

/// Real Ableton Link backend — **not yet implemented**.
///
/// Methods panic with `unimplemented!()` so an accidental `cargo build
/// --features ableton-link` produces a runtime panic with a clear
/// message rather than silently mis-syncing. The CI matrix builds
/// **without** this feature, so the panic can only fire if someone
/// explicitly opts in.
pub struct LinkReal {
    _private: (),
}

impl LinkReal {
    /// Construct the real backend. Currently always panics.
    ///
    /// The v0.2.x follow-up will replace this with the actual
    /// `rust_link::Link::new(initial_bpm)` call + a peer-event listener
    /// thread.
    pub fn new(_initial_bpm: f32) -> Self {
        unimplemented!(
            "ableton-link feature is scaffolded but the real Link binding is not yet wired. \
             See ADR-007 §v0.2 + ADR-009 (LGPL licensing). Use LinkStub for now."
        )
    }
}

impl PeerClock for LinkReal {
    fn current_tempo(&self) -> f32 {
        unimplemented!("LinkReal::current_tempo: see ADR-007 §v0.2 — full impl deferred to v0.2.x")
    }

    fn set_local_tempo(&self, _bpm: f32) {
        unimplemented!(
            "LinkReal::set_local_tempo: see ADR-007 §v0.2 — full impl deferred to v0.2.x"
        )
    }

    fn peer_count(&self) -> usize {
        unimplemented!("LinkReal::peer_count: see ADR-007 §v0.2 — full impl deferred to v0.2.x")
    }

    fn beat_offset_seconds(&self) -> f64 {
        unimplemented!(
            "LinkReal::beat_offset_seconds: see ADR-007 §v0.2 — full impl deferred to v0.2.x"
        )
    }
}
