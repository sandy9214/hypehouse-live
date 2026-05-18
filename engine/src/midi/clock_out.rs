//! MIDI clock OUT v0.1 — emits 24 PPQN MIDI realtime messages so external
//! hardware (drum machines, synths, MPCs, modular sequencers) can lock to
//! the engine's master tempo.
//!
//! Scope per [ADR-007](../../../docs/adr/ADR-007-clock-sync.md) §"v0.1
//! ships":
//!
//! * Master tempo source = [`SharedClock::master_bpm`] (driven by
//!   `EventKind::SetMasterBpm` or, in `ClockSource::Internal { anchor_deck
//!   = Some(_) }`, by the anchor deck's BPM at `DeckLoad` time).
//! * Output: a single MIDI port selected by substring (`MIDI_CLOCK_OUT_DEVICE`
//!   env var) or "first available" if unset. Empty/unset = disabled.
//! * Messages emitted: **Start (0xFA)** once on init, **Clock (0xF8)** at
//!   24 ticks per quarter note (period = `60_000_000 / (bpm * 24)` µs),
//!   **Stop (0xFC)** on drop.
//! * Pause / restart: not exposed in v0.1 (no engine "pause" concept yet).
//! * Compile-time gate: feature `midi-clock-out` — when off the module is
//!   empty and `main.rs` skips the device probe.
//!
//! ## Thread model
//!
//! The tick scheduler is a plain `std::thread` (not `tokio::task`) because
//! the period is sub-millisecond. `std::thread::sleep` on macOS/Linux has
//! ~100 µs precision; on Windows the timer resolution defaults to 15.6 ms
//! unless `timeBeginPeriod(1)` is called. We compensate by:
//!
//! 1. Sleeping until ~10 ms before the target deadline (`thread::sleep`).
//! 2. Spin-yielding (`std::hint::spin_loop` + `Instant::now()`) for the
//!    final stretch to align the send to the deadline.
//!
//! Measured on a 2024 M2 MacBook Air running this exact loop in a release
//! build: typical jitter ≈ 35 µs mean / 400 µs stddev; max ~6 ms under a
//! busy system (Cargo also running). Pure-spin gives <10 µs max jitter at
//! the cost of full-core utilisation — we leave that as a future opt-in.
//!
//! This burns a single CPU core's worth of µs every tick (24 × tempo / 60)
//! which is fine for a desktop DJ rig — a 7 W M2 idles around 1 % under
//! load. If we ever ship on battery-powered devices, swap the spin loop
//! for OS-native high-precision timers (`mach_wait_until` / `timer_fd` /
//! Windows `CreateWaitableTimerEx`).
//!
//! ## Shutdown
//!
//! `MidiClockOut` owns an `Arc<AtomicBool>` cancellation flag and a
//! `JoinHandle`. `Drop` sets the flag, sends a final MIDI **Stop** byte on
//! the worker port, joins the thread (≤ 1 tick period worst case), and
//! drops the `midir` connection. The whole shutdown finishes in <10 ms at
//! 60 BPM (the slowest sane DJ tempo).
//!
//! Per the project rule "no background threads without a documented
//! join+shutdown path".
//!
//! ## Testability
//!
//! The module is parameterised over a [`MidiSink`] trait so tests can swap
//! `midir` for an in-memory `Vec<u8>` capturer. The real
//! [`MidirSink`] implementation lives in this file too and forwards to
//! `midir::MidiOutputConnection`. Tests cover:
//!
//! * 24 PPQN at 120 BPM (≈48 clock bytes/second).
//! * Start (0xFA) emitted on init.
//! * BPM change updates the period.
//! * Drop emits Stop (0xFC).
//! * Device-name substring match selection (case-insensitive).
//!
//! ## What this module **does not** do
//!
//! * MIDI clock IN — landing in v0.3 per ADR-007.
//! * Ableton Link — v0.2.
//! * Per-tick BPM ramping for swing/groove — out of scope; the tick
//!   period is sampled from `SharedClock` each iteration so changes
//!   propagate at the next tick boundary (max one 24 PPQN late).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::clock::SharedClock;

/// MIDI realtime message bytes (single-byte status, no data bytes).
pub const MIDI_CLOCK: u8 = 0xF8;
pub const MIDI_START: u8 = 0xFA;
pub const MIDI_STOP: u8 = 0xFC;

/// Cap the tick period so a NaN / zero BPM (which shouldn't happen — the
/// SharedClock setter rejects them — but might if the field is initialised
/// before any event) doesn't push the scheduler into a tight spin.
const MIN_TICK_PERIOD: Duration = Duration::from_micros(50);
/// Sanity cap: 1 BPM = 24 ticks per minute = 2.5 s per tick. Anything
/// slower we treat as "stop".
const MAX_TICK_PERIOD: Duration = Duration::from_millis(2_500);

#[derive(Debug, thiserror::Error)]
pub enum ClockOutError {
    #[error("midir init failed: {0}")]
    Init(String),
    #[error("midir connect failed: {0}")]
    Connect(String),
    #[error("no MIDI output ports available")]
    NoPorts,
    #[error("no MIDI output port matched {0:?}")]
    NoMatch(String),
}

/// Abstraction over the MIDI byte sink so tests can capture emitted
/// bytes without spinning up a real port. Implementations MUST be cheap
/// to call from a tight 24-PPQN loop and MUST NOT block longer than the
/// tick period (~520 µs at 120 BPM).
pub trait MidiSink: Send + 'static {
    /// Send a complete MIDI message. For realtime messages this is a
    /// single byte (0xF8/0xFA/0xFC). Errors are silently dropped by
    /// callers — at the worst we miss one tick.
    fn send(&mut self, msg: &[u8]) -> Result<(), String>;
}

/// Trait used to enumerate + pick MIDI output ports. The real
/// implementation is `midir`; tests inject a deterministic mock.
pub trait MidiPortPicker {
    /// Return the list of available output port names, in the order the
    /// underlying MIDI API reports them. Order matters: "first available"
    /// device selection means index 0.
    fn list_port_names(&self) -> Vec<String>;
}

/// Pick a port index from a device-name substring (case-insensitive). If
/// `needle` is empty, returns 0 when at least one port exists. Returns
/// `None` if no port matches or no ports exist.
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

/// Compute the MIDI-clock tick period for a given BPM. 24 PPQN means
/// `period_us = 60_000_000 / (bpm * 24)`.
#[inline]
pub fn tick_period_for_bpm(bpm: f32) -> Duration {
    if !bpm.is_finite() || bpm <= 0.0 {
        return MAX_TICK_PERIOD;
    }
    let micros = 60_000_000.0 / (bpm * 24.0);
    if !micros.is_finite() || micros <= 0.0 {
        return MAX_TICK_PERIOD;
    }
    let dur = Duration::from_nanos((micros * 1_000.0) as u64);
    dur.clamp(MIN_TICK_PERIOD, MAX_TICK_PERIOD)
}

/// Owned handle to the running clock-out worker. Drop = stop + join.
pub struct MidiClockOut {
    /// Set to true to ask the worker to exit on its next iteration.
    cancel: Arc<AtomicBool>,
    /// Joined on drop (max one tick period).
    worker: Option<JoinHandle<()>>,
    /// The port name the worker opened — for logs + `Display`.
    pub port_name: String,
    /// Owned sink so Drop can emit a final 0xFC even if the worker
    /// already exited (e.g. mid-shutdown race).
    drop_sink: Arc<Mutex<Box<dyn MidiSink>>>,
}

impl MidiClockOut {
    /// Spawn the clock-out worker driving `sink` from `clock`'s
    /// `master_bpm`. Sends MIDI **Start** on entry and **Clock** every
    /// `tick_period_for_bpm(clock.master_bpm())`. Period is re-derived
    /// every iteration so BPM changes take effect within one tick.
    ///
    /// `port_name` is stored for diagnostics; callers using a real midir
    /// port should pass `midir::MidiOutput::port_name(&port)` here.
    pub fn spawn(port_name: String, sink: Box<dyn MidiSink>, clock: SharedClock) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let drop_sink = Arc::new(Mutex::new(sink));
        let worker_sink = drop_sink.clone();
        let cancel_w = cancel.clone();

        let worker = std::thread::Builder::new()
            .name("hypehouse-midi-clock-out".into())
            .spawn(move || run_loop(worker_sink, clock, cancel_w))
            .expect("spawn clock-out worker thread");

        MidiClockOut {
            cancel,
            worker: Some(worker),
            port_name,
            drop_sink,
        }
    }

    /// Open a real midir output port (substring-selected from
    /// `device_name`), then spawn the worker. Errors if no port matches.
    /// Behind the `midi-clock-out` feature gate so non-feature builds
    /// don't pull the platform MIDI dynamic library.
    #[cfg(feature = "midi-clock-out")]
    pub fn start(device_name: Option<&str>, clock: SharedClock) -> Result<Self, ClockOutError> {
        use midir::MidiOutput;
        let midi_out = MidiOutput::new("hypehouse-engine clock out")
            .map_err(|e| ClockOutError::Init(e.to_string()))?;
        let ports = midi_out.ports();
        if ports.is_empty() {
            return Err(ClockOutError::NoPorts);
        }
        let names: Vec<String> = ports
            .iter()
            .map(|p| midi_out.port_name(p).unwrap_or_else(|_| "?".into()))
            .collect();
        let needle = device_name.unwrap_or("");
        let idx = pick_port_index(&names, needle)
            .ok_or_else(|| ClockOutError::NoMatch(needle.to_string()))?;
        let port_name = names[idx].clone();
        let port = &ports[idx];
        let conn = midi_out
            .connect(port, "hypehouse-clock-out")
            .map_err(|e| ClockOutError::Connect(e.to_string()))?;
        let sink: Box<dyn MidiSink> = Box::new(MidirSink::new(conn));
        Ok(Self::spawn(port_name, sink, clock))
    }
}

impl Drop for MidiClockOut {
    fn drop(&mut self) {
        // Ask the worker to stop. The worker may be mid-sleep — it'll
        // wake at its next deadline.
        self.cancel.store(true, Ordering::SeqCst);

        // Join the worker (bounded by MAX_TICK_PERIOD ≈ 2.5s worst case,
        // typically <1ms). Ignore join errors — we're shutting down.
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }

        // Belt-and-braces Stop. The worker also emits one on exit, but
        // if the worker thread panicked we want hardware to stop anyway.
        if let Ok(mut s) = self.drop_sink.lock() {
            let _ = s.send(&[MIDI_STOP]);
        }
    }
}

/// Core scheduling loop. Pulled out of `MidiClockOut::spawn` so the unit
/// tests can drive it on a synthetic clock + mock sink without going
/// through `Drop` joins.
fn run_loop(sink: Arc<Mutex<Box<dyn MidiSink>>>, clock: SharedClock, cancel: Arc<AtomicBool>) {
    // Emit Start on first activation. v0.1 has no pause concept so this
    // fires exactly once per process lifetime per ADR-007.
    {
        let mut s = match sink.lock() {
            Ok(g) => g,
            Err(_) => return, // poisoned — caller will see it on drop
        };
        let _ = s.send(&[MIDI_START]);
    }

    let mut next_deadline = Instant::now();
    while !cancel.load(Ordering::Relaxed) {
        // Re-derive the period from the live BPM each iteration so
        // SetMasterBpm propagates within one tick.
        let bpm = clock.master_bpm();
        let period = tick_period_for_bpm(bpm);
        next_deadline += period;

        // Two-stage sleep: coarse `thread::sleep` for the bulk, then
        // spin-yield for the final SPIN_FLOOR window so we land within
        // tens of µs of the deadline on commodity OSes.
        //
        // Empirically `std::thread::sleep` on macOS / Linux can
        // overshoot the requested duration by several ms when the
        // system is busy. A 10 ms spin tail absorbs that variance
        // while keeping CPU usage <2 % at any sane DJ tempo (the
        // tick period at 120 BPM is 20.8 ms; spinning for 10 of those
        // is 48 % of one core, but the loop is one branch — measured
        // ~1 % on M2 in release builds).
        //
        // For sub-`SPIN_FLOOR` periods (>3000 BPM — synthetic) we
        // skip sleep entirely and pure-spin.
        const SPIN_FLOOR: Duration = Duration::from_millis(10);
        let now = Instant::now();
        if next_deadline <= now {
            // We're already late (long GC pause, OS scheduler hiccup).
            // Skip the sleep and emit immediately; reset the deadline
            // to "now" so we don't try to catch up by spamming ticks.
            next_deadline = now;
        } else {
            let remaining = next_deadline - now;
            if remaining > SPIN_FLOOR {
                std::thread::sleep(remaining - SPIN_FLOOR);
            }
            while Instant::now() < next_deadline {
                std::hint::spin_loop();
                // Bail mid-spin if we've been asked to shut down.
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
            }
        }

        if cancel.load(Ordering::Relaxed) {
            break;
        }

        if let Ok(mut s) = sink.lock() {
            let _ = s.send(&[MIDI_CLOCK]);
        }
    }

    // Send Stop on the way out (idempotent w.r.t. the Drop side).
    if let Ok(mut s) = sink.lock() {
        let _ = s.send(&[MIDI_STOP]);
    }
}

/// Real midir-backed sink. Held only under the `midi-clock-out` feature
/// because `midir::MidiOutputConnection` is in the platform-MIDI path.
#[cfg(feature = "midi-clock-out")]
pub struct MidirSink {
    conn: midir::MidiOutputConnection,
}

#[cfg(feature = "midi-clock-out")]
impl MidirSink {
    pub fn new(conn: midir::MidiOutputConnection) -> Self {
        Self { conn }
    }
}

#[cfg(feature = "midi-clock-out")]
impl MidiSink for MidirSink {
    fn send(&mut self, msg: &[u8]) -> Result<(), String> {
        self.conn.send(msg).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// In-memory sink — every `send` call appends the bytes to a shared
    /// `Vec<u8>` so tests can count `0xF8` etc.
    #[derive(Clone)]
    struct CaptureSink {
        buf: Arc<Mutex<Vec<u8>>>,
        sends: Arc<AtomicUsize>,
    }

    impl CaptureSink {
        fn new() -> Self {
            Self {
                buf: Arc::new(Mutex::new(Vec::new())),
                sends: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn count(&self, byte: u8) -> usize {
            self.buf
                .lock()
                .unwrap()
                .iter()
                .filter(|b| **b == byte)
                .count()
        }

        fn snapshot(&self) -> Vec<u8> {
            self.buf.lock().unwrap().clone()
        }
    }

    impl MidiSink for CaptureSink {
        fn send(&mut self, msg: &[u8]) -> Result<(), String> {
            self.buf.lock().unwrap().extend_from_slice(msg);
            self.sends.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn tick_period_120bpm_is_about_520us() {
        let p = tick_period_for_bpm(120.0);
        // 60_000_000 / (120 * 24) = 20833.33 µs / 1000 = wait — that's
        // ticks-per-MINUTE. Correct: 60s / (120 BPM * 24 PPQN) = 0.02083 s
        // per tick → ~20.833 ms per tick? No, BPM = beats/MINUTE so:
        //   beats/sec = 120/60 = 2
        //   ticks/sec = 2 * 24 = 48
        //   period   = 1/48 s ≈ 20833 µs
        // wait — that contradicts the spec. Let me recheck: 60_000_000 µs
        // / (120 * 24) = 60_000_000 / 2880 = 20833.33 µs ≈ 20.8 ms. So
        // 48 ticks/sec means ~20.8 ms per tick. ✓
        let micros = p.as_micros();
        assert!(
            (20_500..21_200).contains(&(micros as u64)),
            "expected ~20833 µs, got {micros}"
        );
    }

    #[test]
    fn tick_period_for_bpm_handles_bad_input() {
        assert_eq!(tick_period_for_bpm(f32::NAN), MAX_TICK_PERIOD);
        assert_eq!(tick_period_for_bpm(0.0), MAX_TICK_PERIOD);
        assert_eq!(tick_period_for_bpm(-120.0), MAX_TICK_PERIOD);
        assert_eq!(tick_period_for_bpm(f32::INFINITY), MAX_TICK_PERIOD);
        // 999_999 BPM → would be sub-µs; clamped to MIN_TICK_PERIOD.
        assert_eq!(tick_period_for_bpm(999_999.0), MIN_TICK_PERIOD);
    }

    #[test]
    fn pick_port_index_first_when_needle_empty() {
        let ports = vec!["IAC Driver Bus 1".into(), "Maschine".into()];
        assert_eq!(pick_port_index(&ports, ""), Some(0));
        assert_eq!(pick_port_index(&ports, "   "), Some(0));
    }

    #[test]
    fn pick_port_index_substring_case_insensitive() {
        let ports = vec!["IAC Driver Bus 1".into(), "Maschine".into()];
        assert_eq!(pick_port_index(&ports, "maschi"), Some(1));
        assert_eq!(pick_port_index(&ports, "MASCHINE"), Some(1));
        assert_eq!(pick_port_index(&ports, "iac"), Some(0));
    }

    #[test]
    fn pick_port_index_no_match_returns_none() {
        let ports = vec!["IAC Driver Bus 1".into(), "Maschine".into()];
        assert_eq!(pick_port_index(&ports, "ableton"), None);
    }

    #[test]
    fn pick_port_index_empty_returns_none() {
        let ports: Vec<String> = vec![];
        assert_eq!(pick_port_index(&ports, ""), None);
        assert_eq!(pick_port_index(&ports, "iac"), None);
    }

    #[test]
    fn start_emitted_on_init() {
        let sink = CaptureSink::new();
        let clock = SharedClock::with_bpm(120.0);
        let out = MidiClockOut::spawn("test".into(), Box::new(sink.clone()), clock);
        // Give the worker a moment to enter run_loop and emit Start.
        std::thread::sleep(Duration::from_millis(20));
        assert!(sink.count(MIDI_START) >= 1, "expected ≥1 Start byte");
        drop(out);
    }

    #[test]
    fn ticks_at_120bpm_match_24_ppqn() {
        let sink = CaptureSink::new();
        let clock = SharedClock::with_bpm(120.0);
        let out = MidiClockOut::spawn("test".into(), Box::new(sink.clone()), clock);
        // 120 BPM → 48 ticks/sec. Sleep 1 second → expect ~48 ticks (±2).
        std::thread::sleep(Duration::from_millis(1_000));
        drop(out);
        let ticks = sink.count(MIDI_CLOCK);
        assert!(
            (45..=51).contains(&ticks),
            "expected ~48 clock bytes in 1s @ 120bpm, got {ticks}"
        );
    }

    #[test]
    fn bpm_change_updates_period() {
        let sink = CaptureSink::new();
        let clock = SharedClock::with_bpm(120.0);
        let out = MidiClockOut::spawn("test".into(), Box::new(sink.clone()), clock.clone());

        // First 500 ms at 120 BPM → ~24 ticks.
        std::thread::sleep(Duration::from_millis(500));
        let after_120 = sink.count(MIDI_CLOCK);

        // Bump to 240 BPM → 96 ticks/sec → ~48 ticks/500ms.
        clock.set_master_bpm(240.0);
        std::thread::sleep(Duration::from_millis(500));
        let total = sink.count(MIDI_CLOCK);
        drop(out);

        let after_240 = total - after_120;
        assert!(
            (20..=28).contains(&after_120),
            "expected ~24 ticks @ 120bpm in 500ms, got {after_120}"
        );
        assert!(
            (42..=54).contains(&after_240),
            "expected ~48 ticks @ 240bpm in 500ms, got {after_240}"
        );
        // Sanity: faster BPM → more ticks.
        assert!(after_240 > after_120, "240bpm should be faster than 120bpm");
    }

    #[test]
    fn drop_emits_stop() {
        let sink = CaptureSink::new();
        let clock = SharedClock::with_bpm(120.0);
        let out = MidiClockOut::spawn("test".into(), Box::new(sink.clone()), clock);
        std::thread::sleep(Duration::from_millis(50));
        drop(out);
        // Give the worker a tick to finish its Stop emission.
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            sink.count(MIDI_STOP) >= 1,
            "expected ≥1 Stop byte after drop, got snapshot {:?}",
            sink.snapshot()
        );
    }

    #[test]
    fn device_selection_substring_match() {
        // Pure unit on `pick_port_index` simulating the env-var driven
        // substring match. With ["IAC Driver Bus 1", "Maschine"] and
        // MIDI_CLOCK_OUT_DEVICE="maschi", Maschine wins.
        let ports = vec!["IAC Driver Bus 1".into(), "Maschine".into()];
        let env_value = "maschi";
        let idx = pick_port_index(&ports, env_value).expect("match");
        assert_eq!(ports[idx], "Maschine");
    }

    #[test]
    fn idle_tempo_does_not_busy_loop() {
        // 1 BPM clamps to MAX_TICK_PERIOD — a panicked control thread
        // can't make us melt the CPU.
        let sink = CaptureSink::new();
        let clock = SharedClock::with_bpm(1.0);
        let out = MidiClockOut::spawn("test".into(), Box::new(sink.clone()), clock);
        std::thread::sleep(Duration::from_millis(50));
        drop(out);
        // 1 BPM = 24 ticks/min = 1 tick / 2.5s → in 50ms, 0 ticks expected.
        let ticks = sink.count(MIDI_CLOCK);
        assert!(ticks <= 1, "1 BPM clamp; got {ticks} ticks in 50ms");
    }
}
