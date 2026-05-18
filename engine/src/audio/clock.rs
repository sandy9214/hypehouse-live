//! Engine clock — the monotonic sample-frame counter both threads agree on.
//!
//! ADR-004 §"Open implementation questions" notes we should "trust cpal's
//! `OutputCallbackInfo.timestamp.callback`" for jitter-free monotonic
//! frame numbers. For v0.1 we maintain a software-incremented counter
//! that the audio thread bumps by `frames_in_buffer` on every callback,
//! and we publish it via [`SharedClock`] (an `Arc<AtomicU64>`) so the
//! control thread can read it lock-free.
//!
//! `SharedClock::frame()` uses `Ordering::Relaxed` because the control
//! thread reading "current frame" only needs a recent value — not a
//! synchronization fence. The ring buffer itself synchronizes the
//! commands.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Default master BPM seeded into a fresh [`SharedClock`] — matches the
/// default chosen in ADR-007 ("Open questions").
pub const DEFAULT_MASTER_BPM: f32 = 120.0;

/// Which tempo source is currently driving `master_bpm`. Surfaced to the
/// UI on every `engine.state_changed` notification so a "BPM LOCKED"
/// badge can render when an external master is active. Stored as a
/// single byte inside [`SharedClock`] — the MIDI clock-IN callback flips
/// it to `MidiIn` on a 0xFA Start, back to `Internal` on a 0xFC Stop,
/// and the future Ableton Link backend will flip it to `AbletonLink`
/// when the engine joins a peer session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ClockSource {
    /// Engine drives its own master_bpm — the default at boot.
    Internal = 0,
    /// External MIDI clock master is currently feeding `master_bpm`.
    MidiIn = 1,
    /// Ableton Link peer session is driving `master_bpm` (ADR-007 §v0.2 future).
    AbletonLink = 2,
}

impl ClockSource {
    /// Stable kebab-case wire label. Mirrored on the UI store as a
    /// string union; keep the values in lockstep with
    /// `ui/src/store/engine.ts ClockSourceLabel`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ClockSource::Internal => "internal",
            ClockSource::MidiIn => "midi_in",
            ClockSource::AbletonLink => "ableton_link",
        }
    }

    /// Decode a byte (read off the atomic) into the enum. Unknown
    /// values fall back to `Internal` — defensive, since the byte
    /// originates from our own writes but a future variant could land
    /// out of order during a rolling deploy.
    pub fn from_byte(b: u8) -> Self {
        match b {
            1 => ClockSource::MidiIn,
            2 => ClockSource::AbletonLink,
            _ => ClockSource::Internal,
        }
    }
}

/// Constant audio-thread parameters + the live frame counter pointer.
#[derive(Clone)]
pub struct EngineClock {
    pub sample_rate: u32,
    /// Master BPM at the time the clock was created. Updated through
    /// `AudioCommand::Pitch` etc., but this field on the clock is the
    /// session's nominal master tempo.
    pub master_bpm: f32,
    /// Master phase in radians, used by the v0.1 sine-oscillator mixer.
    pub master_phase: f32,
    /// Shared sample-frame counter. Bumped by the audio thread; read by
    /// the control thread for command scheduling.
    pub shared: SharedClock,
}

impl EngineClock {
    pub fn new(sample_rate: u32, master_bpm: f32) -> Self {
        Self {
            sample_rate,
            master_bpm,
            master_phase: 0.0,
            shared: SharedClock::with_bpm(master_bpm),
        }
    }

    /// Convenience: read the current frame.
    #[inline]
    pub fn frame(&self) -> u64 {
        self.shared.frame()
    }

    /// Convenience: advance the frame (audio thread only).
    #[inline]
    pub fn advance(&self, by: u32) {
        self.shared.advance(by);
    }
}

/// Cheap-to-clone handle to the atomic frame counter + master BPM.
/// Cloning just bumps the `Arc` refcounts; no synchronization.
///
/// The BPM field is stored as the `f32`'s bit pattern in an
/// [`AtomicU32`] so the MIDI-clock-out tick thread (ADR-007 §v0.1) can
/// re-derive the 24 PPQN period without a mutex on every iteration.
#[derive(Clone)]
pub struct SharedClock {
    inner: Arc<SharedClockInner>,
}

struct SharedClockInner {
    /// Monotonic sample-frame counter.
    frame: AtomicU64,
    /// Master BPM, encoded as `f32::to_bits()`. Updated by the control
    /// thread when a `SetMasterBpm` event fires or when the anchor deck
    /// (ADR-007) updates its tempo. Read by both the audio thread and
    /// the MIDI clock OUT tick thread.
    bpm_bits: AtomicU32,
    /// Active tempo source, encoded as a [`ClockSource`] discriminant.
    /// Default = `Internal`. The MIDI clock-IN callback flips this to
    /// `MidiIn` on a Start byte (0xFA) and back to `Internal` on a Stop
    /// byte (0xFC). The bridge samples it once per `state_changed`
    /// notification so the UI lock-indicator stays in sync without
    /// adding a separate event channel.
    clock_source: AtomicU8,
}

impl Default for SharedClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedClock {
    pub fn new() -> Self {
        Self::with_bpm(DEFAULT_MASTER_BPM)
    }

    /// Create a clock seeded with the given master BPM.
    pub fn with_bpm(bpm: f32) -> Self {
        Self {
            inner: Arc::new(SharedClockInner {
                frame: AtomicU64::new(0),
                bpm_bits: AtomicU32::new(bpm.to_bits()),
                clock_source: AtomicU8::new(ClockSource::Internal as u8),
            }),
        }
    }

    /// Read the current frame. `Relaxed` is fine — the control thread
    /// scheduling commands needs a recent value, not a hard fence.
    #[inline]
    pub fn frame(&self) -> u64 {
        self.inner.frame.load(Ordering::Relaxed)
    }

    /// Advance the frame counter by `by` frames. **Audio thread only.**
    /// `Relaxed` because the consumer side (control thread) only reads
    /// for scheduling; the ring buffer carries the actual command
    /// ordering.
    #[inline]
    pub fn advance(&self, by: u32) {
        self.inner.frame.fetch_add(by as u64, Ordering::Relaxed);
    }

    /// Read the current master BPM. Lock-free; the MIDI clock OUT tick
    /// thread calls this every tick to re-derive the period.
    #[inline]
    pub fn master_bpm(&self) -> f32 {
        f32::from_bits(self.inner.bpm_bits.load(Ordering::Relaxed))
    }

    /// Set the master BPM (control thread or audio thread via
    /// `SetMasterBpm` event). Non-finite or <= 0 inputs are ignored so
    /// the MIDI clock OUT period never goes to infinity / NaN.
    #[inline]
    pub fn set_master_bpm(&self, bpm: f32) {
        if bpm.is_finite() && bpm > 0.0 {
            self.inner.bpm_bits.store(bpm.to_bits(), Ordering::Relaxed);
        }
    }

    /// Read the active tempo source. Lock-free; the bridge thread
    /// samples this once per outgoing `engine.state_changed` so the UI
    /// lock-indicator badge stays current without a separate channel.
    #[inline]
    pub fn clock_source(&self) -> ClockSource {
        ClockSource::from_byte(self.inner.clock_source.load(Ordering::Relaxed))
    }

    /// Flip the active tempo source. Called by the MIDI clock-IN
    /// callback on Start / Stop, and (future v0.2) by the Ableton Link
    /// backend on session join / leave.
    #[inline]
    pub fn set_clock_source(&self, src: ClockSource) {
        self.inner.clock_source.store(src as u8, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_clock_starts_zero_and_advances() {
        let c = SharedClock::new();
        assert_eq!(c.frame(), 0);
        c.advance(256);
        assert_eq!(c.frame(), 256);
        c.advance(256);
        assert_eq!(c.frame(), 512);
    }

    #[test]
    fn engine_clock_carries_sample_rate_and_bpm() {
        let c = EngineClock::new(48_000, 120.0);
        assert_eq!(c.sample_rate, 48_000);
        assert!((c.master_bpm - 120.0).abs() < 1e-6);
        assert_eq!(c.frame(), 0);
        c.advance(128);
        assert_eq!(c.frame(), 128);
    }

    #[test]
    fn shared_clock_clones_share_storage() {
        let c = SharedClock::new();
        let c2 = c.clone();
        c.advance(10);
        assert_eq!(c2.frame(), 10);
        c2.advance(5);
        assert_eq!(c.frame(), 15);
    }

    #[test]
    fn shared_clock_bpm_round_trip() {
        let c = SharedClock::with_bpm(128.5);
        assert!((c.master_bpm() - 128.5).abs() < 1e-6);
        c.set_master_bpm(174.0);
        assert!((c.master_bpm() - 174.0).abs() < 1e-6);
    }

    #[test]
    fn shared_clock_rejects_bad_bpm() {
        let c = SharedClock::with_bpm(120.0);
        c.set_master_bpm(f32::NAN);
        c.set_master_bpm(f32::INFINITY);
        c.set_master_bpm(0.0);
        c.set_master_bpm(-30.0);
        assert!((c.master_bpm() - 120.0).abs() < 1e-6);
    }

    #[test]
    fn engine_clock_seeds_shared_bpm() {
        let c = EngineClock::new(48_000, 140.0);
        assert!((c.shared.master_bpm() - 140.0).abs() < 1e-6);
    }

    #[test]
    fn shared_clock_defaults_to_internal_source() {
        let c = SharedClock::new();
        assert_eq!(c.clock_source(), ClockSource::Internal);
        // Wire label is the kebab-case string the UI mirrors.
        assert_eq!(c.clock_source().as_str(), "internal");
    }

    #[test]
    fn shared_clock_source_round_trips_across_clones() {
        // Cheap-clone sharing must extend to the source byte — both
        // sides see the same atomic. Otherwise the MIDI-IN callback
        // would flip its private copy and the bridge sampler would
        // never see it.
        let c = SharedClock::new();
        let c2 = c.clone();
        c.set_clock_source(ClockSource::MidiIn);
        assert_eq!(c2.clock_source(), ClockSource::MidiIn);
        assert_eq!(c2.clock_source().as_str(), "midi_in");
        c2.set_clock_source(ClockSource::AbletonLink);
        assert_eq!(c.clock_source(), ClockSource::AbletonLink);
        assert_eq!(c.clock_source().as_str(), "ableton_link");
        c.set_clock_source(ClockSource::Internal);
        assert_eq!(c2.clock_source(), ClockSource::Internal);
    }
}
