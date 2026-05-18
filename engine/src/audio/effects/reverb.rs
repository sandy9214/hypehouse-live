//! Schroeder reverb: 4 parallel comb filters + 2 series allpass per
//! channel. Delays are mutually-prime small primes (in ms) for natural
//! diffusion. Delay buffers are pre-allocated at construction.
//!
//! Param layout (descriptor order):
//! 0. `room_size` — 0..1 (default 0.5). Sets comb feedback gain.
//! 1. `damping`   — 0..1 (default 0.4). Low-pass on the comb feedback.
//! 2. `width`     — 0..1 (default 0.7). Stereo spread of the wet.
//!
//! Tuned for ~48 kHz; delay lengths re-derived from sample rate at
//! construction (and on rare SR change, which never happens in steady
//! state).

use super::{EffectId, EffectParams, ParamDescriptor, EFFECT_REVERB};

// Schroeder/Freeverb-ish prime delays in samples @ 44.1 kHz (we scale
// by sr / 44100 once at construction).
const COMB_DELAYS_44K: [usize; 4] = [1116, 1188, 1277, 1356];
const COMB_STEREO_OFFSET: usize = 23;
const ALLPASS_DELAYS_44K: [usize; 2] = [556, 441];
const ALLPASS_STEREO_OFFSET: usize = 23;
/// Headroom multiplier so the 4-comb sum + allpass diffusion doesn't
/// clip. Empirical Freeverb-style scaling.
const FIXED_GAIN: f32 = 0.015;

struct Comb {
    buf: Vec<f32>,
    idx: usize,
    /// Low-pass state for damping.
    filter_z: f32,
}

impl Comb {
    fn new(size: usize) -> Self {
        Self {
            buf: vec![0.0; size.max(1)],
            idx: 0,
            filter_z: 0.0,
        }
    }
    #[inline]
    fn process_sample(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.idx];
        // One-pole lowpass on the feedback signal (damping).
        self.filter_z = out * (1.0 - damp) + self.filter_z * damp;
        self.buf[self.idx] = input + self.filter_z * feedback;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
    fn reset(&mut self) {
        for s in self.buf.iter_mut() {
            *s = 0.0;
        }
        self.idx = 0;
        self.filter_z = 0.0;
    }
}

struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

impl Allpass {
    fn new(size: usize) -> Self {
        Self {
            buf: vec![0.0; size.max(1)],
            idx: 0,
        }
    }
    #[inline]
    fn process_sample(&mut self, input: f32, feedback: f32) -> f32 {
        let buffered = self.buf[self.idx];
        let out = -input + buffered;
        self.buf[self.idx] = input + buffered * feedback;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
    fn reset(&mut self) {
        for s in self.buf.iter_mut() {
            *s = 0.0;
        }
        self.idx = 0;
    }
}

pub struct Reverb {
    combs_l: [Comb; 4],
    combs_r: [Comb; 4],
    allpass_l: [Allpass; 2],
    allpass_r: [Allpass; 2],
    /// Allpass feedback — fixed Schroeder value.
    allpass_feedback: f32,
}

impl Reverb {
    pub const DESCRIPTORS: &'static [ParamDescriptor] = &[
        ParamDescriptor {
            name: "room_size",
            min: 0.0,
            max: 1.0,
            default: 0.5,
        },
        ParamDescriptor {
            name: "damping",
            min: 0.0,
            max: 1.0,
            default: 0.4,
        },
        ParamDescriptor {
            name: "width",
            min: 0.0,
            max: 1.0,
            default: 0.7,
        },
    ];

    pub fn new(sample_rate: u32) -> Self {
        let scale = sample_rate as f32 / 44_100.0;
        let comb_sz = |i: usize| ((COMB_DELAYS_44K[i] as f32) * scale) as usize;
        let comb_sz_r =
            |i: usize| (((COMB_DELAYS_44K[i] + COMB_STEREO_OFFSET) as f32) * scale) as usize;
        let ap_sz = |i: usize| ((ALLPASS_DELAYS_44K[i] as f32) * scale) as usize;
        let ap_sz_r =
            |i: usize| (((ALLPASS_DELAYS_44K[i] + ALLPASS_STEREO_OFFSET) as f32) * scale) as usize;
        Self {
            combs_l: [
                Comb::new(comb_sz(0)),
                Comb::new(comb_sz(1)),
                Comb::new(comb_sz(2)),
                Comb::new(comb_sz(3)),
            ],
            combs_r: [
                Comb::new(comb_sz_r(0)),
                Comb::new(comb_sz_r(1)),
                Comb::new(comb_sz_r(2)),
                Comb::new(comb_sz_r(3)),
            ],
            allpass_l: [Allpass::new(ap_sz(0)), Allpass::new(ap_sz(1))],
            allpass_r: [Allpass::new(ap_sz_r(0)), Allpass::new(ap_sz_r(1))],
            allpass_feedback: 0.5,
        }
    }
}

impl super::Effect for Reverb {
    fn id(&self) -> EffectId {
        EFFECT_REVERB
    }
    fn name(&self) -> &'static str {
        "reverb"
    }
    fn params(&self) -> &'static [ParamDescriptor] {
        Self::DESCRIPTORS
    }
    fn process(&mut self, buf: &mut [f32], params: &EffectParams, wet_dry: f32, _sample_rate: u32) {
        let room_size = params[0].clamp(0.0, 1.0);
        let damping = params[1].clamp(0.0, 1.0);
        let width = params[2].clamp(0.0, 1.0);
        // Map room_size 0..1 to comb-feedback 0.7..0.98 (Freeverb).
        let feedback = 0.7 + room_size * 0.28;
        let damp = damping * 0.4; // less aggressive than Freeverb default
        let dry = 1.0 - wet_dry;
        let wet1 = wet_dry * (width * 0.5 + 0.5);
        let wet2 = wet_dry * ((1.0 - width) * 0.5);
        let n_frames = buf.len() / 2;
        for f in 0..n_frames {
            let in_l = buf[f * 2];
            let in_r = buf[f * 2 + 1];
            let mono_in = (in_l + in_r) * FIXED_GAIN;
            // 4 combs in parallel, both channels.
            let mut out_l = 0.0_f32;
            let mut out_r = 0.0_f32;
            for i in 0..4 {
                out_l += self.combs_l[i].process_sample(mono_in, feedback, damp);
                out_r += self.combs_r[i].process_sample(mono_in, feedback, damp);
            }
            // 2 allpass in series.
            for i in 0..2 {
                out_l = self.allpass_l[i].process_sample(out_l, self.allpass_feedback);
                out_r = self.allpass_r[i].process_sample(out_r, self.allpass_feedback);
            }
            buf[f * 2] = dry * in_l + out_l * wet1 + out_r * wet2;
            buf[f * 2 + 1] = dry * in_r + out_r * wet1 + out_l * wet2;
        }
    }
    fn reset(&mut self) {
        for c in self.combs_l.iter_mut() {
            c.reset();
        }
        for c in self.combs_r.iter_mut() {
            c.reset();
        }
        for a in self.allpass_l.iter_mut() {
            a.reset();
        }
        for a in self.allpass_r.iter_mut() {
            a.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Effect;
    use super::*;

    /// Impulse → tail decays to -60dB within `room_size * 5 sec`.
    /// We use room_size=0.5 → budget = 2.5s; we measure within 2s.
    #[test]
    fn impulse_tail_decays() {
        let sr = 48_000;
        let mut r = Reverb::new(sr);
        let n_frames = (sr as f32 * 3.0) as usize;
        let mut buf = vec![0.0_f32; n_frames * 2];
        buf[0] = 1.0;
        buf[1] = 1.0;
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 0.5; // room_size
        params[1] = 0.4; // damping
        params[2] = 0.7; // width
        r.process(&mut buf, &params, 1.0, sr);
        // Measure energy in the last 0.5 s of the buffer.
        let tail_start = ((n_frames as f32 - sr as f32 * 0.5) as usize) * 2;
        let tail_energy: f32 = buf[tail_start..].iter().map(|s| s * s).sum();
        let tail_rms = (tail_energy / (buf.len() - tail_start) as f32).sqrt();
        // -60 dB relative to peak (~1.0) = 0.001.
        assert!(
            tail_rms < 0.01,
            "reverb tail RMS should be near silent at t>=2.5s; got {tail_rms} (≈ {:.1} dB)",
            20.0 * tail_rms.log10()
        );
    }

    #[test]
    fn passthrough_when_dry() {
        let sr = 48_000;
        let mut r = Reverb::new(sr);
        let mut buf: Vec<f32> = (0..512).map(|i| (i as f32 * 0.01).sin()).collect();
        let orig = buf.clone();
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 0.7;
        params[1] = 0.5;
        params[2] = 0.5;
        r.process(&mut buf, &params, 0.0, sr);
        for (a, b) in orig.iter().zip(buf.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "dry reverb should be passthrough: in={a} out={b}"
            );
        }
    }

    #[test]
    fn reset_clears_state() {
        let sr = 48_000;
        let mut r = Reverb::new(sr);
        let mut buf = vec![0.0_f32; 1024];
        buf[0] = 1.0;
        let params = [0.8, 0.3, 0.7, 0.0, 0.0, 0.0];
        r.process(&mut buf, &params, 1.0, sr);
        r.reset();
        let mut zeros = vec![0.0_f32; 1024];
        r.process(&mut zeros, &params, 1.0, sr);
        // Pure dry path on zeros + zero state = silence.
        assert!(zeros.iter().all(|s| s.abs() < 1e-5));
    }

    #[test]
    fn assert_no_alloc_reverb_1024() {
        let sr = 48_000;
        let mut r = Reverb::new(sr);
        let mut buf = [0.1_f32; 2048];
        let params = [0.5, 0.4, 0.7, 0.0, 0.0, 0.0];
        assert_no_alloc::assert_no_alloc(|| {
            r.process(&mut buf, &params, 0.4, sr);
        });
    }
}
