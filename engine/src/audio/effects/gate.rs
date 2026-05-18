//! Beat-synced gate. Opens / closes the audio in a square-wave pattern
//! aligned to the master clock + master BPM.
//!
//! Param layout (descriptor order):
//! 0. `period_div` — 0..3 (default 1).
//!    0 = 1/2 beat, 1 = 1/4 beat, 2 = 1/8, 3 = 1/16.
//! 1. `duty`       — 0..1 (default 0.5). Fraction of the period the
//!    gate is open.
//!
//! Uses the `SharedClock` (sample-frame counter) the audio thread is
//! bumping every callback to determine the current phase within the
//! gate period. **No new threads** — the gate is a pure function of
//! (frame, bpm, sample_rate, period_div, duty).
//!
//! ## Live master BPM
//!
//! The BPM is **read live** from `SharedClock::master_bpm()` on every
//! `process()` call (single `AtomicU32` load with `Relaxed` ordering,
//! ~1 ns). This means `EventKind::SetMasterBpm` — which goes through
//! the main-thread side-channel writer in `engine::main` — takes
//! effect at the next audio buffer boundary without any explicit
//! command plumbing into the audio thread. The BPM is *not* captured
//! at construction time.

use crate::audio::clock::SharedClock;

use super::{EffectId, EffectParams, ParamDescriptor, EFFECT_GATE};

pub struct Gate {
    /// Shared sample-frame counter + master BPM (clone of mixer's
    /// clock). Cloning the SharedClock just bumps an Arc — no heap on
    /// the audio path. BPM is re-read on every `process()` call so
    /// `SetMasterBpm` events propagate live (within one audio buffer).
    clock: SharedClock,
    /// Locally-advanced frame counter, used in tests where the mixer
    /// isn't bumping the SharedClock.
    local_frame: u64,
    sample_rate: u32,
}

impl Gate {
    pub const DESCRIPTORS: &'static [ParamDescriptor] = &[
        ParamDescriptor {
            name: "period_div",
            min: 0.0,
            max: 3.0,
            default: 1.0,
        },
        ParamDescriptor {
            name: "duty",
            min: 0.0,
            max: 1.0,
            default: 0.5,
        },
    ];

    /// Construct a Gate. `master_bpm` is **only** used as the initial
    /// seed for the shared clock's BPM register in test paths that
    /// build a fresh `SharedClock` per-Gate; production callers share
    /// the engine clock and the BPM has already been seeded there. We
    /// keep the signature for backwards compatibility with PR #31 but
    /// the value is no longer cached on the struct.
    pub fn new(clock: SharedClock, sample_rate: u32, _master_bpm: f32) -> Self {
        Self {
            clock,
            local_frame: 0,
            sample_rate,
        }
    }

    /// Map period_div to a beat fraction:
    ///   0 → 1/2 beat
    ///   1 → 1/4 beat
    ///   2 → 1/8 beat
    ///   3 → 1/16 beat
    #[inline]
    fn fraction_of_beat(period_div: u8) -> f32 {
        match period_div {
            0 => 0.5,
            1 => 0.25,
            2 => 0.125,
            3 => 0.0625,
            _ => 0.25,
        }
    }

    /// Pure helper. Returns 1.0 when the gate is open, 0.0 when closed,
    /// for the supplied absolute sample frame. Public so unit tests can
    /// validate the math without driving the SharedClock.
    pub fn open_at(
        frame: u64,
        sample_rate: u32,
        master_bpm: f32,
        period_div: u8,
        duty: f32,
    ) -> f32 {
        if master_bpm <= 0.0 {
            return 1.0;
        }
        let beat_sec = 60.0 / master_bpm;
        let period_sec = beat_sec * Self::fraction_of_beat(period_div);
        let period_frames = (period_sec * sample_rate as f32) as u64;
        if period_frames == 0 {
            return 1.0;
        }
        let phase_frame = frame % period_frames;
        let open_frames = (duty.clamp(0.0, 1.0) * period_frames as f32) as u64;
        if phase_frame < open_frames {
            1.0
        } else {
            0.0
        }
    }
}

impl super::Effect for Gate {
    fn id(&self) -> EffectId {
        EFFECT_GATE
    }
    fn name(&self) -> &'static str {
        "gate"
    }
    fn params(&self) -> &'static [ParamDescriptor] {
        Self::DESCRIPTORS
    }
    fn process(&mut self, buf: &mut [f32], params: &EffectParams, wet_dry: f32, sample_rate: u32) {
        self.sample_rate = sample_rate;
        let period_div = params[0].round().clamp(0.0, 3.0) as u8;
        let duty = params[1].clamp(0.0, 1.0);
        let dry = 1.0 - wet_dry;
        // Read the live master BPM from the SharedClock. Single
        // `AtomicU32` load with `Relaxed` ordering — no heap, no lock.
        // This is what makes `EventKind::SetMasterBpm` take effect at
        // the next buffer boundary without command-channel plumbing.
        let bpm = self.clock.master_bpm();
        // Snapshot the clock once; advance locally inside the buffer
        // (the SharedClock is bumped by the cpal callback at buffer
        // boundaries, so within one buffer we have to step ourselves).
        let base_frame = self.clock.frame().max(self.local_frame);
        let n_frames = buf.len() / 2;
        for f in 0..n_frames {
            let frame = base_frame + f as u64;
            let g = Self::open_at(frame, sample_rate, bpm, period_div, duty);
            // wet_dry blend the gated signal in.
            buf[f * 2] = dry * buf[f * 2] + wet_dry * (buf[f * 2] * g);
            buf[f * 2 + 1] = dry * buf[f * 2 + 1] + wet_dry * (buf[f * 2 + 1] * g);
        }
        self.local_frame = base_frame + n_frames as u64;
    }
    fn reset(&mut self) {
        self.local_frame = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::super::Effect;
    use super::*;

    /// 120 BPM, period_div=1 (= 1/4 beat = 125ms), duty=0.5:
    /// frames 0..62.5ms full, 62.5..125ms zero.
    #[test]
    fn gate_pattern_matches_spec_120bpm_quarter() {
        let sr: u32 = 48_000;
        let bpm = 120.0_f32;
        // 1/4 of a beat: beat = 0.5s, period = 0.125s = 6000 frames.
        let period_frames = ((60.0 / bpm) * 0.25 * sr as f32) as u64;
        assert_eq!(period_frames, 6000);
        // First 3000 frames open (duty=0.5).
        for f in 0..3000 {
            let g = Gate::open_at(f, sr, bpm, 1, 0.5);
            assert!(
                (g - 1.0).abs() < 1e-9,
                "expected gate open at frame {f}, got {g}"
            );
        }
        for f in 3000..6000 {
            let g = Gate::open_at(f, sr, bpm, 1, 0.5);
            assert!(g.abs() < 1e-9, "expected gate closed at frame {f}, got {g}");
        }
        // Period wraps cleanly.
        let g = Gate::open_at(6000, sr, bpm, 1, 0.5);
        assert!((g - 1.0).abs() < 1e-9);
    }

    #[test]
    fn duty_zero_fully_closes() {
        for f in 0..1000 {
            assert_eq!(Gate::open_at(f, 48_000, 120.0, 1, 0.0), 0.0);
        }
    }

    #[test]
    fn duty_one_always_open() {
        for f in 0..1000 {
            assert!((Gate::open_at(f, 48_000, 120.0, 1, 1.0) - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn process_zeros_buffer_in_closed_phase() {
        let sr: u32 = 48_000;
        let bpm = 120.0_f32;
        let mut g = Gate::new(SharedClock::new(), sr, bpm);
        // Start 3000 frames into the period so the buffer falls
        // entirely in the closed half.
        g.local_frame = 3000;
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 1.0; // 1/4 beat
        params[1] = 0.5; // duty
        let mut buf = vec![1.0_f32; 256];
        g.process(&mut buf, &params, 1.0, sr);
        assert!(buf.iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn passthrough_when_dry() {
        let sr: u32 = 48_000;
        let mut g = Gate::new(SharedClock::new(), sr, 120.0);
        let mut buf: Vec<f32> = (0..512).map(|i| (i as f32 * 0.01).sin()).collect();
        let orig = buf.clone();
        let params = [1.0, 0.5, 0.0, 0.0, 0.0, 0.0];
        g.process(&mut buf, &params, 0.0, sr);
        for (a, b) in orig.iter().zip(buf.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "dry gate should be passthrough: in={a} out={b}"
            );
        }
    }

    #[test]
    fn assert_no_alloc_gate_1024() {
        let sr: u32 = 48_000;
        let mut g = Gate::new(SharedClock::new(), sr, 120.0);
        let mut buf = [1.0_f32; 2048];
        let params = [1.0, 0.5, 0.0, 0.0, 0.0, 0.0];
        assert_no_alloc::assert_no_alloc(|| {
            g.process(&mut buf, &params, 1.0, sr);
        });
    }

    /// Live-BPM contract: changing `SharedClock::set_master_bpm` between
    /// `process()` calls must change the gate's pulse period without any
    /// other action (no re-construction, no command).
    #[test]
    fn gate_period_tracks_live_bpm() {
        let sr: u32 = 48_000;
        // Start at 120 BPM. period_div=1 (1/4 beat) → 6000 frames/period.
        let clock = SharedClock::with_bpm(120.0);
        let mut g = Gate::new(clock.clone(), sr, 120.0);
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 1.0; // 1/4 beat
        params[1] = 0.5; // duty
                         // First block of 6000 stereo frames: at 120 BPM the gate
                         // opens for frames 0..3000, then closes 3000..6000.
                         // Verify the boundary directly via open_at + the live BPM
                         // path: `process()` should zero out the closed half.
        let mut buf = vec![1.0_f32; 6000 * 2];
        g.process(&mut buf, &params, 1.0, sr);
        // Frame 0 is the boundary of an open phase → buf[0] should be 1.0.
        assert!((buf[0] - 1.0).abs() < 1e-9);
        // Frame 2999 still open.
        assert!((buf[2999 * 2] - 1.0).abs() < 1e-9);
        // Frame 3000 closed.
        assert!(buf[3000 * 2].abs() < 1e-9);
        // Frame 5999 still closed.
        assert!(buf[5999 * 2].abs() < 1e-9);

        // Now shift to 140 BPM mid-session. period = 0.25 * 60/140 s
        // = ~107.142ms → 5142 frames. duty=0.5 → open for first 2571
        // frames of each period, closed for the next 2571.
        clock.set_master_bpm(140.0);
        let expected_period = ((60.0_f32 / 140.0) * 0.25 * sr as f32) as u64;
        assert_eq!(expected_period, 5142);
        // Reset frame state so the next block starts on a fresh period.
        // (The previous block consumed exactly one period @ 120 BPM, so
        // we just reset the local counter to align tests deterministically.)
        g.reset();
        let mut buf2 = vec![1.0_f32; (expected_period as usize) * 2];
        g.process(&mut buf2, &params, 1.0, sr);
        // Period is now shorter → frame 2571 must be closed (was open
        // under 120-BPM math; under 140-BPM math half-period is 2571).
        assert!(buf2[2571 * 2].abs() < 1e-9);
        // Frame 2570 still open.
        assert!((buf2[2570 * 2] - 1.0).abs() < 1e-9);
        // Frame 0 open at boundary.
        assert!((buf2[0] - 1.0).abs() < 1e-9);
    }

    /// Defensive guard: a malformed `SharedClock` (e.g. one freshly
    /// constructed before main seeded a BPM) returning 0 BPM must not
    /// panic, divide-by-zero, or produce NaN. Gate falls back to
    /// "passthrough = always open" per the `open_at` contract.
    #[test]
    fn gate_with_zero_bpm_doesnt_panic() {
        let sr: u32 = 48_000;
        // SharedClock rejects bad BPMs in `set_master_bpm`, so the
        // only way to read 0 is to wrap `open_at` directly.
        // Still: verify the helper path. (See `open_at` body for the
        // bpm<=0 early-return that yields 1.0.)
        for f in 0..256 {
            let g = Gate::open_at(f, sr, 0.0, 1, 0.5);
            assert!((g - 1.0).abs() < 1e-9);
            let g_neg = Gate::open_at(f, sr, -42.0, 1, 0.5);
            assert!((g_neg - 1.0).abs() < 1e-9);
        }
        // End-to-end via process(): SharedClock seeded to a valid
        // BPM, then we feed a buffer and verify no NaN/Inf escapes
        // even at extreme params.
        let mut g = Gate::new(SharedClock::with_bpm(1.0), sr, 120.0);
        let params = [3.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let mut buf = vec![0.5_f32; 256];
        g.process(&mut buf, &params, 1.0, sr);
        assert!(buf.iter().all(|s| s.is_finite()));
    }

    /// Spec: 120 BPM, period_div=2 (1/8 beat) = 62.5 ms; duty 0.5 →
    /// pulse on for 31.25 ms then off for 31.25 ms. At 48 kHz that's
    /// 3000 frames per period, 1500 frames open / 1500 closed.
    #[test]
    fn gate_period_div_eighth_at_120bpm() {
        let sr: u32 = 48_000;
        let bpm = 120.0_f32;
        // 1/8 of a beat: beat = 0.5s, period = 0.0625s = 3000 frames.
        let period_frames = ((60.0 / bpm) * 0.125 * sr as f32) as u64;
        assert_eq!(period_frames, 3000);
        // First 1500 frames open (duty=0.5).
        for f in 0..1500 {
            let g = Gate::open_at(f, sr, bpm, 2, 0.5);
            assert!(
                (g - 1.0).abs() < 1e-9,
                "expected gate open at frame {f}, got {g}"
            );
        }
        for f in 1500..3000 {
            let g = Gate::open_at(f, sr, bpm, 2, 0.5);
            assert!(g.abs() < 1e-9, "expected gate closed at frame {f}, got {g}");
        }
        // Period wraps.
        let g = Gate::open_at(3000, sr, bpm, 2, 0.5);
        assert!((g - 1.0).abs() < 1e-9);
    }

    /// BPM doubling halves the period: a frame that was *open* at 120
    /// BPM (frame 2999 of a 6000-frame period @ duty 0.5 → still
    /// inside first half) must be *closed* once BPM jumps to 240 (new
    /// period 3000, frame 2999 falls into the second half of period 0
    /// = closed). Verify the math via `open_at` + via end-to-end live
    /// BPM swap.
    #[test]
    fn gate_bpm_doubling_halves_period() {
        let sr: u32 = 48_000;
        // 120 BPM, frame 2999 → open (first half of 6000-frame period).
        assert!((Gate::open_at(2999, sr, 120.0, 1, 0.5) - 1.0).abs() < 1e-9);
        // 240 BPM, frame 2999 → closed (second half of 3000-frame period).
        assert!(Gate::open_at(2999, sr, 240.0, 1, 0.5).abs() < 1e-9);
        // End-to-end: SharedClock + process().
        let clock = SharedClock::with_bpm(120.0);
        let mut g = Gate::new(clock.clone(), sr, 120.0);
        let params = [1.0, 0.5, 0.0, 0.0, 0.0, 0.0];
        let mut buf = vec![1.0_f32; 3000 * 2];
        g.process(&mut buf, &params, 1.0, sr);
        assert!((buf[2999 * 2] - 1.0).abs() < 1e-9);
        // Swap BPM mid-session, reset so we start a fresh period.
        clock.set_master_bpm(240.0);
        g.reset();
        let mut buf2 = vec![1.0_f32; 3000 * 2];
        g.process(&mut buf2, &params, 1.0, sr);
        assert!(buf2[2999 * 2].abs() < 1e-9);
    }

    /// Micro-benchmark: average per-call latency over 1M iterations on
    /// a 1024-stereo-frame buffer. Compare against the same test on
    /// the pre-refactor code path to bound the regression of reading
    /// `clock.master_bpm()` each call. `#[ignore]` so CI doesn't run
    /// the 1M-iter loop on every PR.
    #[test]
    #[ignore]
    fn bench_gate_process_1024() {
        use std::time::Instant;
        let sr: u32 = 48_000;
        let mut g = Gate::new(SharedClock::with_bpm(120.0), sr, 120.0);
        let mut buf = [0.1_f32; 2048];
        let params = [1.0, 0.5, 0.0, 0.0, 0.0, 0.0];
        for _ in 0..10_000 {
            g.process(&mut buf, &params, 1.0, sr);
        }
        let n: u32 = 1_000_000;
        let t = Instant::now();
        for _ in 0..n {
            g.process(&mut buf, &params, 1.0, sr);
        }
        let total_ns = t.elapsed().as_nanos();
        eprintln!("bench_gate_avg_ns_per_1024_frames={}", total_ns / n as u128);
    }

    /// Live BPM is read on every `process()` call — no allocation
    /// regression from the AtomicU32 load. Use the assert_no_alloc
    /// crate to prove it stays heap-free even after the refactor.
    #[test]
    fn gate_live_bpm_no_alloc() {
        let sr: u32 = 48_000;
        let clock = SharedClock::with_bpm(120.0);
        let mut g = Gate::new(clock.clone(), sr, 120.0);
        let mut buf = [1.0_f32; 2048];
        let params = [1.0, 0.5, 0.0, 0.0, 0.0, 0.0];
        assert_no_alloc::assert_no_alloc(|| {
            g.process(&mut buf, &params, 1.0, sr);
            clock.set_master_bpm(140.0);
            g.process(&mut buf, &params, 1.0, sr);
            clock.set_master_bpm(174.0);
            g.process(&mut buf, &params, 1.0, sr);
        });
    }
}
