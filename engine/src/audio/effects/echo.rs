//! Echo effect: delay line with stereo cross-feedback and a one-pole
//! tone tilt on the feedback path.
//!
//! Param layout (descriptor order):
//! 0. `time_ms`  — 10..2000 ms (default 250)
//! 1. `feedback` — 0..0.95 (default 0.45)
//! 2. `tone`     — -1..+1 (default 0). Negative darkens (LP), positive
//!    brightens (HP). Applied to the feedback signal so each repeat is
//!    progressively colored.
//!
//! Delay-line is pre-allocated for 2s @ 96 kHz max (192 000 frames ×
//! stereo = 384 000 f32 = ~1.5 MB on the audio thread). Allocation
//! happens once in `new()`; the audio-thread `process()` only reads /
//! writes existing slots.

use super::{EffectId, EffectParams, ParamDescriptor, EFFECT_ECHO};

/// Max delay we support — 2 s × 96 kHz × stereo = 384 000 samples.
const MAX_DELAY_FRAMES: usize = 192_000;

pub struct Echo {
    /// Pre-allocated stereo ring buffer (interleaved L, R, L, R, …).
    delay_l: Vec<f32>,
    delay_r: Vec<f32>,
    /// Write head in frames (mono index).
    write_head: usize,
    /// One-pole tone-tilt state (per channel) on the feedback path.
    tone_z_l: f32,
    tone_z_r: f32,
    /// Track sample rate so we can resize the delay line on rare SR
    /// change. `new()` provisions for the configured SR; the buffer
    /// stays `MAX_DELAY_FRAMES` regardless.
    sample_rate: u32,
}

impl Echo {
    pub const DESCRIPTORS: &'static [ParamDescriptor] = &[
        ParamDescriptor {
            name: "time_ms",
            min: 10.0,
            max: 2000.0,
            default: 250.0,
        },
        ParamDescriptor {
            name: "feedback",
            min: 0.0,
            max: 0.95,
            default: 0.45,
        },
        ParamDescriptor {
            name: "tone",
            min: -1.0,
            max: 1.0,
            default: 0.0,
        },
    ];

    pub fn new(sample_rate: u32) -> Self {
        Self {
            delay_l: vec![0.0; MAX_DELAY_FRAMES],
            delay_r: vec![0.0; MAX_DELAY_FRAMES],
            write_head: 0,
            tone_z_l: 0.0,
            tone_z_r: 0.0,
            sample_rate,
        }
    }
}

impl super::Effect for Echo {
    fn id(&self) -> EffectId {
        EFFECT_ECHO
    }
    fn name(&self) -> &'static str {
        "echo"
    }
    fn params(&self) -> &'static [ParamDescriptor] {
        Self::DESCRIPTORS
    }
    fn process(&mut self, buf: &mut [f32], params: &EffectParams, wet_dry: f32, sample_rate: u32) {
        self.sample_rate = sample_rate;
        let time_ms = params[0].clamp(10.0, 2000.0);
        let feedback = params[1].clamp(0.0, 0.95);
        let tone = params[2].clamp(-1.0, 1.0);
        let delay_frames = ((time_ms / 1000.0) * sample_rate as f32) as usize;
        let delay_frames = delay_frames.clamp(1, MAX_DELAY_FRAMES - 1);
        // Tone coefficient — simple one-pole IIR.
        //   tone > 0: highpass-ish (smaller a → less low-end retention)
        //   tone < 0: lowpass-ish  (larger a → smoother)
        // a in (0, 1). Map tone ∈ [-1, +1] → a ∈ [0.95, 0.0]:
        //   -1 → a=0.95 (heavy LP), +1 → a=0.0 (no smoothing).
        let a = ((1.0 - tone) * 0.5).clamp(0.0, 0.95);
        let dry = 1.0 - wet_dry;
        let n_frames = buf.len() / 2;
        let mask = MAX_DELAY_FRAMES; // not power-of-2; use modulo.
        for f in 0..n_frames {
            let in_l = buf[f * 2];
            let in_r = buf[f * 2 + 1];
            // Read tap is at (write_head - delay_frames) mod buffer.
            let read_idx = (self.write_head + mask - delay_frames) % mask;
            let tap_l = self.delay_l[read_idx];
            let tap_r = self.delay_r[read_idx];
            // Apply tone tilt to feedback signal. One-pole IIR:
            //   y[n] = (1-a)*x[n] + a*y[n-1]
            self.tone_z_l = (1.0 - a) * tap_l + a * self.tone_z_l;
            self.tone_z_r = (1.0 - a) * tap_r + a * self.tone_z_r;
            // Cross-feedback for width: L gets a bit of R's tap and
            // vice versa. 0.7/0.3 split keeps stereo image while
            // smearing into the opposite channel.
            let fb_l = self.tone_z_l * 0.7 + self.tone_z_r * 0.3;
            let fb_r = self.tone_z_r * 0.7 + self.tone_z_l * 0.3;
            // Write input + feedback back into delay line.
            self.delay_l[self.write_head] = in_l + fb_l * feedback;
            self.delay_r[self.write_head] = in_r + fb_r * feedback;
            // Mix dry + wet.
            buf[f * 2] = dry * in_l + wet_dry * tap_l;
            buf[f * 2 + 1] = dry * in_r + wet_dry * tap_r;
            self.write_head = (self.write_head + 1) % mask;
        }
    }
    fn reset(&mut self) {
        for s in self.delay_l.iter_mut() {
            *s = 0.0;
        }
        for s in self.delay_r.iter_mut() {
            *s = 0.0;
        }
        self.write_head = 0;
        self.tone_z_l = 0.0;
        self.tone_z_r = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::super::Effect;
    use super::*;

    /// 500ms delay produces a recognizable second-tap at 500ms.
    #[test]
    fn produces_delayed_tap() {
        let sr = 48_000;
        let mut e = Echo::new(sr);
        let delay_ms = 500.0_f32;
        let total_frames = (sr as f32 * (delay_ms / 1000.0 * 2.0)) as usize; // 1 s of audio
        let mut buf = vec![0.0_f32; total_frames * 2];
        // Single-sample impulse at frame 0.
        buf[0] = 1.0;
        buf[1] = 1.0;
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = delay_ms;
        params[1] = 0.0; // no feedback → exactly one tap
        params[2] = 0.0; // neutral tone
        e.process(&mut buf, &params, 1.0, sr);
        // Expect a peak near frame = sr * 0.5 = 24000.
        let expected_frame = (sr as f32 * delay_ms / 1000.0) as usize;
        let window = 16;
        let start = expected_frame.saturating_sub(window);
        let end = (expected_frame + window).min(buf.len() / 2);
        let mut max_in_window: f32 = 0.0;
        for f in start..end {
            let m = (buf[f * 2].abs()).max(buf[f * 2 + 1].abs());
            if m > max_in_window {
                max_in_window = m;
            }
        }
        assert!(
            max_in_window > 0.5,
            "expected a tap of ≥0.5 near frame {expected_frame}, got max {max_in_window}"
        );
    }

    #[test]
    fn passthrough_when_dry() {
        let sr = 48_000;
        let mut e = Echo::new(sr);
        let mut buf: Vec<f32> = (0..512).map(|i| (i as f32 * 0.01).sin()).collect();
        let orig = buf.clone();
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 100.0;
        params[1] = 0.5;
        params[2] = 0.0;
        e.process(&mut buf, &params, 0.0, sr);
        for (a, b) in orig.iter().zip(buf.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "dry echo should be passthrough: in={a} out={b}"
            );
        }
    }

    #[test]
    fn feedback_zero_yields_single_tap() {
        let sr = 48_000;
        let mut e = Echo::new(sr);
        let mut buf = vec![0.0_f32; sr as usize * 2];
        buf[0] = 1.0;
        buf[1] = 1.0;
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 200.0;
        params[1] = 0.0;
        params[2] = 0.0;
        e.process(&mut buf, &params, 1.0, sr);
        // 2nd-loop tap (at 400ms) should be ~0.
        let second_loop_frame = (sr as f32 * 0.4) as usize;
        let m = buf[second_loop_frame * 2].abs();
        assert!(m < 0.05, "feedback=0 second tap should be silent, got {m}");
    }

    #[test]
    fn reset_clears_delay_line() {
        let sr = 48_000;
        let mut e = Echo::new(sr);
        let mut buf = vec![0.0_f32; 1024];
        buf[0] = 1.0;
        buf[1] = 1.0;
        let params = [50.0, 0.6, 0.0, 0.0, 0.0, 0.0];
        e.process(&mut buf, &params, 1.0, sr);
        e.reset();
        let mut zeros = vec![0.0_f32; 1024];
        e.process(&mut zeros, &params, 1.0, sr);
        assert!(zeros.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn assert_no_alloc_echo_1024() {
        let sr = 48_000;
        let mut e = Echo::new(sr);
        let mut buf = [0.1_f32; 2048];
        let params = [250.0, 0.45, 0.0, 0.0, 0.0, 0.0];
        assert_no_alloc::assert_no_alloc(|| {
            e.process(&mut buf, &params, 0.5, sr);
        });
    }
}
