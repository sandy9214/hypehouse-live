//! Peer clock sync subsystem — ADR-007 §v0.2 scaffolding.
//!
//! This module is the trait-level home for **peer-to-peer** clock sync
//! protocols (currently: Ableton Link). The MIDI clock IN / OUT modules
//! live under [`crate::midi`] because they ride on the same `midir`
//! transport as our MIDI controller input layer; Link is a separate
//! peer-to-peer LAN protocol with its own multicast + tempo-arbitration
//! model, so it gets its own module.
//!
//! ## Scope of this PR (scaffold only)
//!
//! Per ADR-007 §v0.2, the full Ableton Link integration requires:
//!
//! 1. The official C++ Link SDK (Ableton, LGPL v3) vendored or linked
//!    dynamically.
//! 2. A Rust binding crate (community `rust-link`, or a hand-rolled
//!    `bindgen` wrapper).
//! 3. ADR-009 sign-off on the LGPL licensing implications.
//!
//! All three are non-trivial and warrant their own PR. This module
//! lands the **trait abstraction + a stub default impl** so the rest of
//! the engine can be written against [`PeerClock`] today, and the real
//! implementation can be slotted in behind the `ableton-link` Cargo
//! feature without touching call sites.
//!
//! ## Module layout
//!
//! * [`PeerClock`] — the trait every backend implements.
//! * [`LinkStub`]  — the default backend; returns sensible no-op values
//!   and logs a one-time warning so operators know Link isn't actually
//!   wired yet.
//! * [`link_real`] — placeholder for the real `rust-link`-backed
//!   implementation. Behind the `ableton-link` feature flag; currently
//!   `unimplemented!()` per ADR-007 §v0.2 (full impl deferred to
//!   v0.2.x).
//!
//! ## Why a trait, not a concrete type?
//!
//! Two reasons:
//!
//! 1. **Compile-time gating.** The real impl pulls in C++ + LGPL code.
//!    Most users won't want that on their build matrix. The trait lets
//!    us swap in [`LinkStub`] for them without `#[cfg]` peppering at
//!    call sites.
//! 2. **Testability.** Unit tests can implement [`PeerClock`] in-process
//!    with a deterministic [`std::sync::Mutex`] without ever touching
//!    UDP multicast.

pub mod link_stub;

#[cfg(feature = "ableton-link")]
pub mod link_real;

pub use link_stub::LinkStub;

/// Abstraction over a peer-to-peer clock sync backend (Ableton Link
/// today; ProDJ Link in a hypothetical future).
///
/// Implementations MUST be `Send + Sync` because the engine shares the
/// backend across the audio thread, the control thread, and the
/// WebSocket bridge (which surfaces peer count + tempo to the UI).
///
/// All methods are non-blocking — they read atomics / interior-mutable
/// fields and return immediately. A real-time-safe contract similar to
/// [`crate::audio::clock::SharedClock`].
pub trait PeerClock: Send + Sync {
    /// Current session tempo as seen by the peer-clock backend, in BPM.
    ///
    /// For the stub this is always [`link_stub::STUB_TEMPO`] (= 120.0).
    /// For the real Link impl this is the network-arbitrated session
    /// tempo (Link runs a voting algorithm across peers).
    fn current_tempo(&self) -> f32;

    /// Push a new local tempo into the peer-clock backend. Other peers
    /// on the LAN will see this value and may adopt it (Link's tempo
    /// arbitration favors the most recently changed value by default).
    ///
    /// Stub impl logs the call and discards. Real impl forwards to
    /// `link_real::Link::set_tempo`.
    fn set_local_tempo(&self, bpm: f32);

    /// Number of OTHER peers currently visible on the LAN (does not
    /// include self). Stub returns 0.
    ///
    /// Surfaced to the UI as the "Link" peer-count badge.
    fn peer_count(&self) -> usize;

    /// Beat-phase offset relative to the shared session clock, in
    /// seconds. Used for sub-beat phase sync — e.g. nudging the engine
    /// frame counter so beat 1 of our session lines up with beat 1 of
    /// every other Link-aware app on the LAN.
    ///
    /// Stub returns 0.0 (no offset = perfectly in phase with itself).
    fn beat_offset_seconds(&self) -> f64;
}
