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

use crate::audio::clock::SharedClock;

use super::{EffectId, EffectParams, ParamDescriptor, EFFECT_GATE};

pub struct Gate {
    /// Shared sample-frame counter (clone of mixer's clock). Cloning
    /// the SharedClock just bumps an Arc — no heap on the audio path.
    clock: SharedClock,
    /// Locally-advanced frame counter, used in tests where the mixer
    /// isn't bumping the SharedClock.
    local_frame: u64,
    /// Master tempo in BPM. Updated via the `master_bpm` field —
    /// could be plumbed from a `Pitch` audio command in a follow-up.
    master_bpm: f32,
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

    pub fn new(clock: SharedClock, sample_rate: u32, master_bpm: f32) -> Self {
        Self {
            clock,
            local_frame: 0,
            master_bpm,
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
        // Snapshot the clock once; advance locally inside the buffer
        // (the SharedClock is bumped by the cpal callback at buffer
        // boundaries, so within one buffer we have to step ourselves).
        let base_frame = self.clock.frame().max(self.local_frame);
        let n_frames = buf.len() / 2;
        for f in 0..n_frames {
            let frame = base_frame + f as u64;
            let g = Self::open_at(frame, sample_rate, self.master_bpm, period_div, duty);
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
}
