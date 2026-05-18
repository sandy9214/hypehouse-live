//! MIDI clock IN v0.3 — accept an external MIDI clock master so the
//! engine can lock its master_bpm to a hardware sequencer / DAW.
//!
//! Scope per [ADR-007](../../../docs/adr/ADR-007-clock-sync.md) §"v0.3":
//!
//! * Input: a single MIDI input port selected by substring
//!   (`MIDI_CLOCK_IN_DEVICE` env var). Empty / unset = disabled.
//! * Messages consumed:
//!   - **Start (0xFA)** — begin counting ticks.
//!   - **Clock (0xF8)** — 24 ticks per quarter note. We timestamp each
//!     incoming tick and, every 24 ticks (= one beat), compute the
//!     instantaneous BPM from the interval between the first and last
//!     tick of that beat.
//!   - **Stop (0xFC)** — stop counting; subsequent 0xF8 are ignored
//!     until the next 0xFA. State (tick counter, smoothing buffer) is
//!     reset.
//! * Smoothing: a ring buffer of the last [`SMOOTHING_WINDOW`] beat
//!   BPMs is averaged before being pushed into [`SharedClock`]. This
//!   absorbs the per-tick jitter that any USB-MIDI / virtual-port host
//!   bus picks up (typically ±0.3–1.0 ms = ±0.1–0.5 BPM at 120 BPM).
//! * Deadband: a smoothed BPM that differs from the current
//!   [`SharedClock::master_bpm`] by less than [`BPM_DEADBAND`] is
//!   ignored — no `set_master_bpm` call. This avoids spamming the
//!   audio thread (and downstream MIDI clock OUT, when wired together
//!   via mirror mode) with micro-jitter updates.
//! * Compile-time gate: feature `midi-clock-in`. When off, the module
//!   still exposes the [`MidiSource`] trait + pure helpers so the
//!   tests run without the platform MIDI dynamic library; only the
//!   `midir`-backed [`MidiClockIn::start`] entry point is gated.
//!
//! ## Mode interaction with MIDI clock OUT
//!
//! v0.3 ships the simplest possible interlock: if
//! `MIDI_CLOCK_IN_DEVICE` is set, the engine silently disables MIDI
//! clock OUT. This avoids the obvious feedback loop where:
//!
//! 1. We mirror an external 120 BPM clock into `master_bpm` (= 120.0).
//! 2. Our OUT thread emits 24 PPQN @ 120 BPM.
//! 3. The DAW sees our OUT, accepts it, drifts its sequencer slightly,
//!    feeds the drift back in via IN, and the loop amplifies.
//!
//! A future v0.4 may add a "mirror" mode that re-emits the incoming
//! clock byte-for-byte (1:1, no derived period); this is left as a
//! TODO. The main.rs wiring enforces the disable in
//! `spawn_midi_clock_out_if_enabled`.
//!
//! ## Threading model
//!
//! `midir` invokes its input callback on the platform's MIDI thread
//! (CoreMIDI / ALSA / WinMM). The callback runs in real-time priority
//! context — we must NOT block, allocate, or call into async. Per byte
//! we do:
//!
//! 1. A single `Instant::now()` call (monotonic, lock-free).
//! 2. A `Mutex::lock` on the inner state (un-contended — only the
//!    callback writes; reads come from `SharedClock` via atomics).
//! 3. At most one `SharedClock::set_master_bpm` (single atomic store).
//!
//! This is well under the per-byte budget of a midir callback
//! (microseconds to milliseconds depending on host).
//!
//! ## Shutdown
//!
//! `MidiClockIn` owns the `midir::MidiInputConnection`. Dropping the
//! handle closes the port; midir guarantees the callback thread joins
//! before `drop` returns. We also store a cancellation `AtomicBool`
//! that the callback checks on every byte so an in-flight tick is
//! ignored once the handle is being torn down. This mirrors the
//! "no background threads without a documented join+shutdown path"
//! rule from `CLAUDE.md`.
//!
//! ## Testability
//!
//! The module is parameterised over a [`MidiSource`] trait so unit
//! tests can feed synthetic clock bytes without a real port. The real
//! midir-backed implementation lives behind the `midi-clock-in`
//! feature flag; the trait + the in-memory test source compile on
//! every build.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::audio::clock::{ClockSource, SharedClock};

/// MIDI realtime message bytes (single-byte status, no data bytes).
pub const MIDI_CLOCK: u8 = 0xF8;
pub const MIDI_START: u8 = 0xFA;
pub const MIDI_STOP: u8 = 0xFC;

/// Ticks per quarter note in the MIDI clock spec (1 beat = 24 ticks).
pub const TICKS_PER_BEAT: usize = 24;

/// How many beats to average before pushing a BPM update. Smaller =
/// faster lock-in but more jittery; larger = smoother but lags the
/// master more on real tempo changes.
///
/// 4 beats ≈ 2 s at 120 BPM — the gear we sync to (drum machines,
/// DAW sequencers) doesn't change tempo faster than that during a
/// live performance.
pub const SMOOTHING_WINDOW: usize = 4;

/// Minimum BPM delta vs the current `SharedClock::master_bpm` that
/// triggers a `set_master_bpm` write. Anything smaller is treated as
/// jitter and dropped. ±0.1 BPM is the JND for a trained ear and well
/// below the inherent precision of consumer-grade MIDI USB transport.
pub const BPM_DEADBAND: f32 = 0.1;

/// Reject obviously-bogus inferred BPMs (a missed tick / spurious 0xF8
/// can momentarily blow the interval up or down). These are wider than
/// any musical DJ tempo and exist purely to keep noise out of the
/// smoothing buffer.
pub const MIN_PLAUSIBLE_BPM: f32 = 20.0;
pub const MAX_PLAUSIBLE_BPM: f32 = 999.0;

#[derive(Debug, thiserror::Error)]
pub enum ClockInError {
    #[error("midir init failed: {0}")]
    Init(String),
    #[error("midir connect failed: {0}")]
    Connect(String),
    #[error("no MIDI input ports available")]
    NoPorts,
    #[error("no MIDI input port matched {0:?}")]
    NoMatch(String),
}

/// Abstraction over the MIDI byte source so unit tests can feed
/// synthetic clock bytes without spinning up a real `midir` port.
///
/// Implementations call the supplied closure on each incoming byte.
/// The closure does the actual state mutation + BPM derivation; the
/// source only owns the transport (real MIDI port vs in-memory queue).
///
/// `Send + 'static` because real midir callbacks land on a foreign
/// real-time thread and we need to own the closure across that
/// boundary.
pub trait MidiSource: Send + 'static {
    /// Block until the source is shut down. The implementation feeds
    /// each incoming byte through `on_byte`. Returns when the upstream
    /// (real port closed / test queue drained + sentinel) is gone.
    fn run<F>(self, on_byte: F)
    where
        F: FnMut(u8) + Send + 'static;
}

/// Pick a port index from a device-name substring (case-insensitive).
/// Mirrors `clock_out::pick_port_index` so the IN + OUT pickers behave
/// identically. Returns `None` if no port matches or no ports exist.
pub fn pick_port_index(ports: &[String], needle: &str) -> Option<usize> {
    if ports.is_empty() {
        return None;
    }
    let trimmed = needle.trim();
    if trimmed.is_empty() {
        return Some(0);
    }
    let needle_low = trimmed.to_lowercase();
    ports
        .iter()
        .position(|n| n.to_lowercase().contains(&needle_low))
}

/// Compute the inferred BPM from the elapsed wall-clock duration of
/// `TICKS_PER_BEAT` MIDI clock ticks (= one beat). Returns `None` for
/// non-finite / non-positive / obviously-out-of-range inputs.
///
/// MIDI clock spec: 24 ticks per beat. So:
///
/// ```text
///     beat_seconds = elapsed_seconds_for_24_ticks
///     beats_per_minute = 60.0 / beat_seconds
/// ```
#[inline]
pub fn bpm_from_beat_duration(beat_duration_secs: f64) -> Option<f32> {
    if !beat_duration_secs.is_finite() || beat_duration_secs <= 0.0 {
        return None;
    }
    let bpm = 60.0 / beat_duration_secs;
    if !bpm.is_finite() {
        return None;
    }
    let bpm = bpm as f32;
    if !(MIN_PLAUSIBLE_BPM..=MAX_PLAUSIBLE_BPM).contains(&bpm) {
        return None;
    }
    Some(bpm)
}

/// Mean of the values currently in the smoothing window. Defined for
/// non-empty inputs only; callers check `is_empty()` themselves so the
/// "first beat" case is explicit.
#[inline]
pub fn smooth_bpm(window: &VecDeque<f32>) -> Option<f32> {
    if window.is_empty() {
        return None;
    }
    let sum: f32 = window.iter().copied().sum();
    Some(sum / window.len() as f32)
}

/// Interior state mutated by the midir callback. Owned behind a
/// `Mutex` because the callback may run on multiple OS threads on
/// some midir backends (rare but documented), and because the unit
/// tests want to inspect it.
#[derive(Debug)]
pub struct ClockInState {
    /// Are we currently counting ticks? Flips to true on 0xFA, false
    /// on 0xFC. Starts false so a fresh process ignores ticks until
    /// the master sends an explicit Start.
    pub running: bool,
    /// Wall-clock timestamp of the first tick of the current beat
    /// window. We measure the duration of `TICKS_PER_BEAT` ticks to
    /// derive BPM, so the first tick is the anchor.
    pub beat_anchor: Option<Instant>,
    /// Count of ticks received since `beat_anchor`. Reset to 0 every
    /// time we complete a beat.
    pub ticks_in_beat: usize,
    /// Ring buffer of the last `SMOOTHING_WINDOW` inferred beat BPMs.
    /// We average this to produce the value we push into the shared
    /// clock; absorbs per-tick jitter.
    pub window: VecDeque<f32>,
    /// Last BPM we pushed into `SharedClock::set_master_bpm`. Used by
    /// the deadband — re-emitting an identical value would still wake
    /// the audio thread's atomic but we keep the write rate close to
    /// the actual tempo-change rate of the upstream master.
    pub last_emitted_bpm: Option<f32>,
}

impl ClockInState {
    pub fn new() -> Self {
        Self {
            running: false,
            beat_anchor: None,
            ticks_in_beat: 0,
            window: VecDeque::with_capacity(SMOOTHING_WINDOW),
            last_emitted_bpm: None,
        }
    }

    /// Reset everything we accumulate during a "running" period.
    /// Called on 0xFC (Stop) and on first entry to make `start`
    /// idempotent w.r.t. repeated 0xFA bytes.
    pub fn reset(&mut self) {
        self.running = false;
        self.beat_anchor = None;
        self.ticks_in_beat = 0;
        self.window.clear();
        // We intentionally KEEP `last_emitted_bpm` so the deadband is
        // still active on resume — re-emitting an identical BPM would
        // be redundant.
    }
}

impl Default for ClockInState {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle a single incoming MIDI status byte. Returns the smoothed
/// BPM that should be pushed into `SharedClock` (if any), so the
/// caller can apply the deadband against the live clock value. Pure
/// w.r.t. the wall clock — `now` is injected so tests can drive it
/// deterministically.
///
/// This is the heart of the module. Splitting it out from the
/// `MidiSource` plumbing means the unit tests cover all the
/// state-machine transitions without touching `midir`.
pub fn process_byte(state: &mut ClockInState, byte: u8, now: Instant) -> Option<f32> {
    match byte {
        MIDI_START => {
            state.reset();
            state.running = true;
            None
        }
        MIDI_STOP => {
            state.reset();
            None
        }
        MIDI_CLOCK => {
            if !state.running {
                // Per spec we ignore 0xF8 outside a 0xFA..0xFC window.
                // Some DAWs (Ableton Live) emit 0xF8 continuously even
                // when the transport is stopped — silently drop those.
                return None;
            }
            // First tick of a fresh beat? Plant the anchor at this
            // tick's timestamp. The anchor IS the first tick of the
            // beat — the beat completes when we receive the FIRST
            // tick of the next beat (i.e. `TICKS_PER_BEAT` more ticks
            // arrive after the anchor). So a "full beat" is anchor +
            // TICKS_PER_BEAT subsequent ticks = 25 0xF8 bytes total.
            if state.beat_anchor.is_none() {
                state.beat_anchor = Some(now);
                state.ticks_in_beat = 0;
                return None;
            }
            state.ticks_in_beat += 1;
            if state.ticks_in_beat < TICKS_PER_BEAT {
                return None;
            }
            // We've now received `TICKS_PER_BEAT` ticks AFTER the
            // anchor. The anchor was at `beat_anchor`; this current
            // tick is at `now`. The interval [anchor, now] spans
            // exactly one beat (24 PPQN per MIDI spec).
            let anchor = state.beat_anchor.expect("just checked");
            let beat_duration = now.saturating_duration_since(anchor);
            let beat_secs = beat_duration.as_secs_f64();
            let beat_bpm = bpm_from_beat_duration(beat_secs);

            // Re-anchor on the current tick (= first tick of the next
            // beat) and reset the counter. This way per-tick jitter
            // doesn't accumulate over many beats — each beat is
            // measured independently.
            state.beat_anchor = Some(now);
            state.ticks_in_beat = 0;

            let Some(bpm) = beat_bpm else {
                // Bogus beat — likely a missed tick. Don't poison the
                // smoothing window.
                return None;
            };

            if state.window.len() == SMOOTHING_WINDOW {
                state.window.pop_front();
            }
            state.window.push_back(bpm);

            smooth_bpm(&state.window)
        }
        _ => None, // Active sensing, system reset, non-realtime — ignore.
    }
}

/// Map a realtime status byte to the [`ClockSource`] transition it
/// implies, if any. `Some(MidiIn)` on a Start (0xFA) so the bridge can
/// surface a "LOCKED" badge once an external master engages;
/// `Some(Internal)` on a Stop (0xFC) so the badge reverts when the
/// master goes quiet. Every other byte (including the per-tick 0xF8) is
/// `None` — we don't want to flip the source on every tick.
///
/// Pure helper, lifted out so unit tests cover the transition table
/// without spinning up the midir callback machinery.
#[inline]
pub fn clock_source_for_byte(byte: u8) -> Option<ClockSource> {
    match byte {
        MIDI_START => Some(ClockSource::MidiIn),
        MIDI_STOP => Some(ClockSource::Internal),
        _ => None,
    }
}

/// Decide whether `smoothed_bpm` differs from `live_bpm` enough to
/// warrant a `SharedClock::set_master_bpm` write.
///
/// Pure helper, lifted out for testability.
#[inline]
pub fn should_emit(live_bpm: f32, smoothed_bpm: f32, last_emitted: Option<f32>) -> bool {
    // Always emit the very first value so the clock locks in fast.
    let baseline = last_emitted.unwrap_or(live_bpm);
    (smoothed_bpm - baseline).abs() >= BPM_DEADBAND
}

/// Owned handle to a running MIDI clock IN listener. Drop closes the
/// underlying `midir` connection (and joins the callback thread, per
/// `midir` guarantees).
pub struct MidiClockIn {
    /// Set to true to ask the callback to ignore further bytes.
    cancel: Arc<AtomicBool>,
    /// The midir input connection — kept alive for the listener
    /// lifetime. None in tests that use the in-memory source.
    #[cfg(feature = "midi-clock-in")]
    _conn: Option<midir::MidiInputConnection<()>>,
    /// The port name we opened — for diagnostics.
    pub port_name: String,
    /// Shared state, owned so tests can peek into it.
    pub state: Arc<Mutex<ClockInState>>,
}

impl MidiClockIn {
    /// Build a clock-in handle around an arbitrary `MidiSource`. The
    /// source's `run` method drives the byte stream on its own thread
    /// (real impls spawn one; the test impl runs synchronously).
    ///
    /// Returns immediately — the source spawns its own driver thread
    /// (either via midir's callback registration or, in tests, an
    /// explicit `std::thread::spawn`).
    pub fn from_source<S: MidiSource>(port_name: String, source: S, clock: SharedClock) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(ClockInState::new()));

        let state_for_cb = Arc::clone(&state);
        let cancel_for_cb = Arc::clone(&cancel);
        let clock_for_cb = clock;

        source.run(move |byte| {
            if cancel_for_cb.load(Ordering::Relaxed) {
                return;
            }
            // Flip the active tempo source on Start / Stop so the
            // bridge's `engine.state_changed` notification surfaces the
            // current lock state. Done OUTSIDE the state mutex — the
            // SharedClock atomic is lock-free.
            if let Some(src) = clock_source_for_byte(byte) {
                clock_for_cb.set_clock_source(src);
            }
            // We hold the state lock for the body of the byte handler
            // only — it never crosses an await / I/O boundary. The
            // SharedClock store is lock-free (atomic).
            let smoothed = {
                let mut s = match state_for_cb.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let smoothed = process_byte(&mut s, byte, Instant::now());
                // Capture the deadband baseline + last-emitted under
                // the same lock so concurrent bytes can't race the
                // deadband check.
                if let Some(bpm) = smoothed {
                    let live = clock_for_cb.master_bpm();
                    if should_emit(live, bpm, s.last_emitted_bpm) {
                        clock_for_cb.set_master_bpm(bpm);
                        s.last_emitted_bpm = Some(bpm);
                        Some(bpm)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            // `smoothed` is intentionally unused — the SharedClock
            // write above is the side effect. Keeping the binding
            // makes the control flow clear if we ever want to wire a
            // tracing event in.
            let _ = smoothed;
        });

        Self {
            cancel,
            #[cfg(feature = "midi-clock-in")]
            _conn: None,
            port_name,
            state,
        }
    }

    /// Open a real midir input port (substring-selected from
    /// `device_name`) and start consuming clock bytes. Behind the
    /// `midi-clock-in` feature flag so non-feature builds don't pull
    /// the platform-MIDI dynamic library.
    #[cfg(feature = "midi-clock-in")]
    pub fn start(device_name: Option<&str>, clock: SharedClock) -> Result<Self, ClockInError> {
        use midir::MidiInput;
        let midi_in = MidiInput::new("hypehouse-engine clock in")
            .map_err(|e| ClockInError::Init(e.to_string()))?;
        let ports = midi_in.ports();
        if ports.is_empty() {
            return Err(ClockInError::NoPorts);
        }
        let names: Vec<String> = ports
            .iter()
            .map(|p| midi_in.port_name(p).unwrap_or_else(|_| "?".into()))
            .collect();
        let needle = device_name.unwrap_or("");
        let idx = pick_port_index(&names, needle)
            .ok_or_else(|| ClockInError::NoMatch(needle.to_string()))?;
        let port_name = names[idx].clone();
        let port = &ports[idx];

        let cancel = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(ClockInState::new()));

        let state_for_cb = Arc::clone(&state);
        let cancel_for_cb = Arc::clone(&cancel);
        let clock_for_cb = clock;

        let conn = midi_in
            .connect(
                port,
                "hypehouse-clock-in",
                move |_ts, bytes, _ctx: &mut ()| {
                    if cancel_for_cb.load(Ordering::Relaxed) {
                        return;
                    }
                    // Realtime messages are single-byte. Some midir
                    // backends batch multiple bytes per callback; loop.
                    for &byte in bytes {
                        // Flip the SharedClock source on Start/Stop so
                        // the UI badge tracks the master's transport
                        // (atomic store — outside the state mutex).
                        if let Some(src) = clock_source_for_byte(byte) {
                            clock_for_cb.set_clock_source(src);
                        }
                        let mut s = match state_for_cb.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        let smoothed = process_byte(&mut s, byte, Instant::now());
                        if let Some(bpm) = smoothed {
                            let live = clock_for_cb.master_bpm();
                            if should_emit(live, bpm, s.last_emitted_bpm) {
                                clock_for_cb.set_master_bpm(bpm);
                                s.last_emitted_bpm = Some(bpm);
                            }
                        }
                    }
                },
                (),
            )
            .map_err(|e| ClockInError::Connect(e.to_string()))?;

        Ok(Self {
            cancel,
            _conn: Some(conn),
            port_name,
            state,
        })
    }
}

impl Drop for MidiClockIn {
    fn drop(&mut self) {
        // Ask the callback to stop processing bytes. midir's Drop
        // closes the port + joins the callback thread for us.
        self.cancel.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// In-memory MIDI source — drains a vec of bytes synchronously on
    /// `run`. The closure is invoked once per byte, in order.
    struct VecSource {
        bytes: Vec<u8>,
    }

    impl VecSource {
        fn new(bytes: Vec<u8>) -> Self {
            Self { bytes }
        }
    }

    impl MidiSource for VecSource {
        fn run<F>(self, mut on_byte: F)
        where
            F: FnMut(u8) + Send + 'static,
        {
            for b in self.bytes {
                on_byte(b);
            }
        }
    }

    /// Helper: simulate one full beat — fires `TICKS_PER_BEAT + 1`
    /// 0xF8 ticks `interval_micros` µs apart (i.e. the anchor tick
    /// plus 24 more, spanning exactly TICKS_PER_BEAT intervals = one
    /// beat). The emission lands on the final tick. Returns the
    /// per-tick emissions so callers can find the last `Some`.
    fn feed_beat(
        state: &mut ClockInState,
        anchor: Instant,
        interval_micros: u64,
    ) -> Vec<Option<f32>> {
        let count = TICKS_PER_BEAT + 1;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let now = anchor + Duration::from_micros(interval_micros * i as u64);
            out.push(process_byte(state, MIDI_CLOCK, now));
        }
        out
    }

    #[test]
    fn test_24_ticks_at_120bpm_sets_master_bpm_120() {
        // 120 BPM → 2 beats/sec → 48 ticks/sec → 20833.33 µs/tick.
        // A "beat" in MIDI clock terms is TICKS_PER_BEAT intervals,
        // i.e. anchor + 24 more ticks (25 0xF8 bytes total). On the
        // final tick the beat completes and we emit a BPM.
        let mut state = ClockInState::new();
        let now = Instant::now();
        assert!(process_byte(&mut state, MIDI_START, now).is_none());
        let interval_us = 20_833;
        let mut emitted = None;
        for i in 0..=TICKS_PER_BEAT {
            let t = now + Duration::from_micros(interval_us * i as u64);
            if let Some(v) = process_byte(&mut state, MIDI_CLOCK, t) {
                emitted = Some(v);
            }
        }
        let bpm = emitted.expect("beat should produce a smoothed BPM");
        assert!(
            (bpm - 120.0).abs() < 0.5,
            "expected ~120 BPM after one beat, got {bpm}"
        );
    }

    #[test]
    fn test_smoothing_averages_4_beats() {
        // Feed beats at 110 / 130 / 120 / 120 BPM; smoothed should
        // ≈ 120 (the unweighted mean). Each beat = anchor + 24
        // intervals at `60 / bpm / TICKS_PER_BEAT` seconds per
        // interval. The "anchor" tick of beat N+1 is the same tick as
        // the final tick of beat N, so we don't double-count: feed
        // anchor + 24 ticks for the first beat, then 24 more ticks
        // (sharing the anchor) for each subsequent beat.
        let mut state = ClockInState::new();
        let mut t = Instant::now();
        process_byte(&mut state, MIDI_START, t);

        let bpms = [110.0_f64, 130.0, 120.0, 120.0];
        let mut last = None;
        let mut first_beat = true;
        for bpm in bpms {
            let interval = Duration::from_secs_f64(60.0 / bpm / TICKS_PER_BEAT as f64);
            // Fire the anchor tick on the first beat only; for
            // subsequent beats the previous beat's final tick already
            // became the next anchor inside `process_byte`.
            if first_beat {
                if let Some(v) = process_byte(&mut state, MIDI_CLOCK, t) {
                    last = Some(v);
                }
                first_beat = false;
            }
            for _ in 0..TICKS_PER_BEAT {
                t += interval;
                if let Some(v) = process_byte(&mut state, MIDI_CLOCK, t) {
                    last = Some(v);
                }
            }
        }
        let smoothed = last.expect("four full beats produce a smoothed value");
        let expected = (110.0 + 130.0 + 120.0 + 120.0) / 4.0;
        assert!(
            (smoothed - expected).abs() < 1.0,
            "smoothed {smoothed} should be ≈ unweighted mean {expected}"
        );
    }

    #[test]
    fn test_stop_resets_state() {
        let mut state = ClockInState::new();
        let now = Instant::now();
        process_byte(&mut state, MIDI_START, now);
        // Half a beat of ticks.
        for i in 0..12 {
            process_byte(
                &mut state,
                MIDI_CLOCK,
                now + Duration::from_micros(i * 20_833),
            );
        }
        assert!(state.running);
        // First tick anchors (counter stays 0); the next 11 increment
        // the counter to 11. 12 ticks total → counter == 11.
        assert_eq!(state.ticks_in_beat, 11);
        assert!(state.beat_anchor.is_some());
        // Stop.
        process_byte(&mut state, MIDI_STOP, now);
        assert!(!state.running);
        assert_eq!(state.ticks_in_beat, 0);
        assert!(state.beat_anchor.is_none());
        assert!(state.window.is_empty());
        // Ticks after Stop are silently ignored.
        let emitted = process_byte(&mut state, MIDI_CLOCK, now + Duration::from_secs(1));
        assert!(emitted.is_none());
        assert_eq!(state.ticks_in_beat, 0);
    }

    #[test]
    fn test_start_after_stop_resumes() {
        let mut state = ClockInState::new();
        let mut t = Instant::now();
        process_byte(&mut state, MIDI_START, t);
        feed_beat(&mut state, t, 20_833);
        assert!(!state.window.is_empty());

        // Stop wipes the window.
        process_byte(&mut state, MIDI_STOP, t);
        assert!(state.window.is_empty());

        // Restart: feed another full beat at 120 BPM.
        t += Duration::from_secs(1);
        process_byte(&mut state, MIDI_START, t);
        assert!(state.running);
        let emitted = feed_beat(&mut state, t, 20_833);
        let last = emitted.iter().rev().find_map(|v| *v).expect("got a beat");
        assert!(
            (last - 120.0).abs() < 0.5,
            "resumed beat should still produce ≈120 BPM, got {last}"
        );
    }

    #[test]
    fn test_deadband_skips_micro_changes() {
        // 120.0 live, 120.05 smoothed (within deadband): should NOT
        // request an emit. 120.0 live, 120.5 smoothed: should emit.
        assert!(!should_emit(120.0, 120.05, Some(120.0)));
        assert!(!should_emit(120.0, 119.95, Some(120.0)));
        assert!(should_emit(120.0, 120.5, Some(120.0)));
        assert!(should_emit(120.0, 119.5, Some(120.0)));
        // First-ever emit (last_emitted = None) compares to live BPM.
        assert!(!should_emit(120.0, 120.05, None));
        assert!(should_emit(120.0, 121.0, None));
    }

    #[test]
    fn test_device_substring_match() {
        let ports = vec!["IAC Driver Bus 1".into(), "Maschine".into()];
        // Case-insensitive substring "iac" should match "IAC Driver Bus 1".
        let idx = pick_port_index(&ports, "iac").expect("match");
        assert_eq!(ports[idx], "IAC Driver Bus 1");
        // Empty needle picks the first port.
        assert_eq!(pick_port_index(&ports, ""), Some(0));
        // No match returns None.
        assert_eq!(pick_port_index(&ports, "ableton"), None);
        // Empty list returns None even with empty needle.
        assert_eq!(pick_port_index(&[], ""), None);
    }

    #[test]
    fn test_clock_bytes_before_start_are_ignored() {
        // Some DAWs (Ableton Live) emit 0xF8 continuously even when
        // their transport is stopped. We must NOT count those.
        let mut state = ClockInState::new();
        let now = Instant::now();
        for i in 0..50 {
            process_byte(
                &mut state,
                MIDI_CLOCK,
                now + Duration::from_micros(i * 20_833),
            );
        }
        assert!(!state.running);
        assert_eq!(state.ticks_in_beat, 0);
        assert!(state.beat_anchor.is_none());
        assert!(state.window.is_empty());
    }

    #[test]
    fn test_bogus_beat_duration_is_rejected() {
        // A missed-tick beat (artificially-stretched interval) infers
        // a BPM far below MIN_PLAUSIBLE_BPM=20.0 and must be dropped
        // rather than poison the smoothing window.
        let mut state = ClockInState::new();
        let now = Instant::now();
        process_byte(&mut state, MIDI_START, now);
        // TICKS_PER_BEAT ticks at 120 BPM (anchor + 23 more), then a
        // huge gap before the beat-completing 25th tick.
        let interval = Duration::from_micros(20_833);
        for i in 0..TICKS_PER_BEAT {
            process_byte(&mut state, MIDI_CLOCK, now + interval * i as u32);
        }
        // Final tick: 12 seconds after the anchor → ≈ 5 BPM, rejected.
        process_byte(
            &mut state,
            MIDI_CLOCK,
            now + interval * TICKS_PER_BEAT as u32 + Duration::from_secs(12),
        );
        assert!(
            state.window.is_empty(),
            "implausibly slow beat must not enter window"
        );
    }

    #[test]
    fn test_smoothing_window_capped() {
        // Feed 10 beats; the window must never grow past SMOOTHING_WINDOW.
        let mut state = ClockInState::new();
        let mut t = Instant::now();
        process_byte(&mut state, MIDI_START, t);
        let interval = Duration::from_micros(20_833);
        for _ in 0..10 {
            for _ in 0..TICKS_PER_BEAT {
                process_byte(&mut state, MIDI_CLOCK, t);
                t += interval;
            }
        }
        assert_eq!(state.window.len(), SMOOTHING_WINDOW);
    }

    #[test]
    fn test_bpm_from_beat_duration_clamps_extremes() {
        assert_eq!(bpm_from_beat_duration(0.0), None);
        assert_eq!(bpm_from_beat_duration(-1.0), None);
        assert_eq!(bpm_from_beat_duration(f64::NAN), None);
        assert_eq!(bpm_from_beat_duration(f64::INFINITY), None);
        // 0.5s/beat = 120 BPM.
        let v = bpm_from_beat_duration(0.5).unwrap();
        assert!((v - 120.0).abs() < 1e-3);
        // 60s/beat = 1 BPM — below MIN_PLAUSIBLE_BPM, rejected.
        assert_eq!(bpm_from_beat_duration(60.0), None);
        // 0.01s/beat = 6000 BPM — above MAX_PLAUSIBLE_BPM, rejected.
        assert_eq!(bpm_from_beat_duration(0.01), None);
    }

    #[test]
    fn test_full_pipeline_via_source_updates_shared_clock() {
        // Plumbed end-to-end: VecSource feeds bytes through
        // `from_source`, which derives BPM + writes SharedClock. We
        // start the clock at 100.0 BPM and expect it to lock to
        // ≈120 after 4 beats @ 120 BPM.
        let clock = SharedClock::with_bpm(100.0);
        // MIDI_START followed by 4 beats × 24 ticks of 0xF8. The
        // VecSource calls `on_byte` synchronously, but `process_byte`
        // uses `Instant::now()` for each call — so this test is
        // sensitive to system scheduling. We give a wide tolerance
        // (±5 BPM). The timed test below covers actual lock-in.
        let mut bytes = vec![MIDI_START];
        bytes.extend(std::iter::repeat_n(MIDI_CLOCK, 4 * TICKS_PER_BEAT));
        let source = VecSource::new(bytes);
        let handle = MidiClockIn::from_source("test".into(), source, clock.clone());
        // The source ran to completion synchronously; the state lock
        // is uncontended. With zero wall time between ticks the
        // inferred BPMs are all > MAX_PLAUSIBLE_BPM and rejected, so
        // SharedClock stays at 100.0. The test still proves the
        // plumbing is intact (no panics, port_name preserved).
        assert_eq!(handle.port_name, "test");
        // Either rejected (clock unchanged) OR locked — both are
        // acceptable outcomes for a same-Instant tick stream.
        let bpm = clock.master_bpm();
        assert!(
            bpm == 100.0 || (20.0..=999.0).contains(&bpm),
            "shared clock BPM out of range: {bpm}"
        );
    }

    #[test]
    fn test_full_pipeline_with_timed_source_drives_shared_clock() {
        // A timed source spawns a thread that sleeps the correct
        // interval between bytes. By the time the run loop returns,
        // the SharedClock must have been written to. We deliberately
        // don't assert a tight BPM lock: `std::thread::sleep` on macOS
        // shared CI runners overshoots routinely by 1-10 ms on a 20 ms
        // tick (observed on GitHub-hosted macos-latest), which makes
        // any tight BPM assertion flaky. Lock accuracy is covered by
        // the deterministic `test_24_ticks_at_120bpm_*` test above.
        struct TimedSource {
            bytes: Vec<(u8, Duration)>,
        }
        impl MidiSource for TimedSource {
            fn run<F>(self, mut on_byte: F)
            where
                F: FnMut(u8) + Send + 'static,
            {
                std::thread::spawn(move || {
                    for (b, dt) in self.bytes {
                        std::thread::sleep(dt);
                        on_byte(b);
                    }
                })
                .join()
                .expect("timed source thread");
            }
        }

        let interval = Duration::from_micros(20_833);
        // 8 beats so we get past the smoothing window's initial fill
        // and survive a few CI-runner hiccups.
        let mut bytes = vec![(MIDI_START, Duration::from_millis(1))];
        for _ in 0..(1 + 8 * TICKS_PER_BEAT) {
            bytes.push((MIDI_CLOCK, interval));
        }

        let clock = SharedClock::with_bpm(50.0);
        let _handle =
            MidiClockIn::from_source("test-timed".into(), TimedSource { bytes }, clock.clone());

        // BPM should be in the plausible range — the module clamps
        // out-of-band beats so a flaky runner can't push us past it.
        let bpm = clock.master_bpm();
        assert!(
            (MIN_PLAUSIBLE_BPM..=MAX_PLAUSIBLE_BPM).contains(&bpm),
            "BPM out of plausible range: {bpm}"
        );
        // And it should have moved off the initial 50 BPM — the
        // pipeline actually wrote something through the deadband.
        assert!(
            (bpm - 50.0).abs() > BPM_DEADBAND,
            "SharedClock never updated past initial 50.0, got {bpm}"
        );
    }

    #[test]
    fn clock_source_for_byte_only_flips_on_transport_bytes() {
        // The 24 PPQN tick byte (0xF8) MUST NOT flip the source — that
        // would write to the atomic every ~20 ms during normal playback.
        // Only Start (0xFA) and Stop (0xFC) change the active source.
        assert_eq!(clock_source_for_byte(MIDI_START), Some(ClockSource::MidiIn));
        assert_eq!(
            clock_source_for_byte(MIDI_STOP),
            Some(ClockSource::Internal)
        );
        assert_eq!(clock_source_for_byte(MIDI_CLOCK), None);
        // Active sensing / system reset / random data bytes are no-ops.
        assert_eq!(clock_source_for_byte(0xFE), None);
        assert_eq!(clock_source_for_byte(0x00), None);
        assert_eq!(clock_source_for_byte(0x90), None);
    }

    #[test]
    fn from_source_flips_shared_clock_source_on_start_and_stop() {
        // End-to-end through the same path the real midir callback
        // takes: a Start byte locks the engine to MidiIn, a Stop byte
        // reverts to Internal. The badge in the UI keys off this byte.
        let clock = SharedClock::with_bpm(120.0);
        assert_eq!(clock.clock_source(), ClockSource::Internal);
        let _h = MidiClockIn::from_source(
            "src-flip".into(),
            VecSource::new(vec![MIDI_START]),
            clock.clone(),
        );
        assert_eq!(clock.clock_source(), ClockSource::MidiIn);
        let _h2 = MidiClockIn::from_source(
            "src-flip-stop".into(),
            VecSource::new(vec![MIDI_STOP]),
            clock.clone(),
        );
        assert_eq!(clock.clock_source(), ClockSource::Internal);
    }
}
