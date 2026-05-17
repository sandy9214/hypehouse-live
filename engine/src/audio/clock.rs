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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
            shared: SharedClock::new(),
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

/// Cheap-to-clone handle to the atomic frame counter. Cloning just
/// bumps the `Arc` refcount; no synchronization.
#[derive(Clone)]
pub struct SharedClock {
    inner: Arc<AtomicU64>,
}

impl Default for SharedClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Read the current frame. `Relaxed` is fine — the control thread
    /// scheduling commands needs a recent value, not a hard fence.
    #[inline]
    pub fn frame(&self) -> u64 {
        self.inner.load(Ordering::Relaxed)
    }

    /// Advance the frame counter by `by` frames. **Audio thread only.**
    /// `Relaxed` because the consumer side (control thread) only reads
    /// for scheduling; the ring buffer carries the actual command
    /// ordering.
    #[inline]
    pub fn advance(&self, by: u32) {
        self.inner.fetch_add(by as u64, Ordering::Relaxed);
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
}
