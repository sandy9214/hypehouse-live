//! `LinkStub` — the default [`PeerClock`] backend.
//!
//! Returns sensible no-op values for every method on the trait and logs
//! a one-time warning the first time the engine constructs one so
//! operators understand the `ableton-link` Cargo feature isn't wired in
//! a follow-up PR yet (ADR-007 §v0.2 scaffold only).
//!
//! ## Design choices
//!
//! * **`Send + Sync` without locking.** All accessors return constants
//!   except [`LinkStub::set_local_tempo`], which is a logged no-op. The
//!   stub holds no mutable state so it's trivially `Sync`.
//! * **One-shot log.** A [`std::sync::Once`] guards the "Ableton Link
//!   stub: feature not yet wired" message so a hot path that constructs
//!   `LinkStub` many times doesn't spam the log.
//! * **No allocation, no I/O.** The stub is safe to construct in the
//!   audio thread if we ever needed to (we won't — the engine builds
//!   the `PeerClock` on the control thread).

use std::sync::Once;

use tracing::{info, warn};

use super::PeerClock;

/// Default session tempo reported by the stub. Matches
/// [`crate::audio::clock::DEFAULT_MASTER_BPM`] so a UI that polls the
/// stub gets the same 120 BPM it gets from the audio clock.
pub const STUB_TEMPO: f32 = 120.0;

/// One-time log guard so repeated `LinkStub::new()` calls don't flood
/// the log with the "not yet wired" warning.
static LOG_ONCE: Once = Once::new();

/// Default `PeerClock` backend. No external state; constructs are free.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinkStub;

impl LinkStub {
    /// Construct a stub backend. Logs the "not yet wired" warning on
    /// the first call only (process-wide).
    pub fn new() -> Self {
        LOG_ONCE.call_once(|| {
            warn!(
                "Ableton Link stub: feature not yet wired; v0.2.x follow-up. \
                 PeerClock methods return defaults (tempo=120, peers=0, offset=0)."
            );
        });
        Self
    }
}

impl PeerClock for LinkStub {
    fn current_tempo(&self) -> f32 {
        STUB_TEMPO
    }

    fn set_local_tempo(&self, bpm: f32) {
        // No real session to push to. Log at info so it shows up in
        // dev / debug runs but doesn't pollute prod stderr.
        info!(
            target = "clock_sync::link_stub",
            requested_bpm = bpm,
            "LinkStub::set_local_tempo: no-op (real backend gated behind `ableton-link` feature)"
        );
    }

    fn peer_count(&self) -> usize {
        0
    }

    fn beat_offset_seconds(&self) -> f64 {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_stub_returns_default_tempo_120() {
        let stub = LinkStub::new();
        assert!(
            (stub.current_tempo() - 120.0).abs() < f32::EPSILON,
            "stub tempo must equal 120 BPM default, got {}",
            stub.current_tempo()
        );
    }

    #[test]
    fn link_stub_set_local_tempo_is_no_op() {
        // The call must not panic / error for any finite or non-finite
        // input — the trait contract is "always non-blocking, never
        // fail". Real backends may clamp / reject; the stub just logs.
        let stub = LinkStub::new();
        stub.set_local_tempo(128.0);
        stub.set_local_tempo(0.0);
        stub.set_local_tempo(-30.0);
        stub.set_local_tempo(f32::NAN);
        stub.set_local_tempo(f32::INFINITY);
        // Reported tempo never changes — the stub has no state.
        assert!((stub.current_tempo() - STUB_TEMPO).abs() < f32::EPSILON);
    }

    #[test]
    fn link_stub_reports_zero_peers() {
        let stub = LinkStub::new();
        assert_eq!(stub.peer_count(), 0);
    }

    #[test]
    fn link_stub_beat_offset_is_zero() {
        let stub = LinkStub::new();
        assert!(stub.beat_offset_seconds().abs() < f64::EPSILON);
    }

    #[test]
    fn link_stub_is_send_sync() {
        // Compile-time check: any `PeerClock` impl must be `Send + Sync`
        // because we share it across threads in `main.rs`.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LinkStub>();
    }

    #[test]
    fn link_stub_is_cheap_to_clone() {
        // The stub is `Copy`, so cloning is a register move — cheaper
        // than an `Arc` bump. Important for the bridge handle that
        // distributes peer-clock readers across many WS clients.
        let stub = LinkStub::new();
        let _a = stub;
        let _b = stub;
        let _c = stub;
        assert_eq!(stub.peer_count(), 0);
    }

    #[test]
    fn link_stub_used_as_trait_object() {
        // Real call sites take `&dyn PeerClock`; make sure that
        // continues to compile + behave correctly.
        let stub = LinkStub::new();
        let dyn_ref: &dyn PeerClock = &stub;
        assert!((dyn_ref.current_tempo() - STUB_TEMPO).abs() < f32::EPSILON);
        assert_eq!(dyn_ref.peer_count(), 0);
        assert!(dyn_ref.beat_offset_seconds().abs() < f64::EPSILON);
        dyn_ref.set_local_tempo(174.0);
    }
}
