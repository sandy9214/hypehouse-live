//! Filter effect: 12dB biquad LP / HP / BP, RBJ coefficients.
//!
//! Param layout (descriptor order):
//! 0. `cutoff_hz` — 20..20000 Hz (default 500)
//! 1. `resonance` — 0..1 (default 0.3). Mapped to Q ∈ [0.707, ~8].
//! 2. `mode`      — 0..2 (LP=0, HP=1, BP=2; default 0)
//!
//! Two separate biquad instances run (L + R) so the filter is true
//! stereo. Coefficients are recomputed only when the cutoff / Q / mode
//! changes — branch-free fast path otherwise.

use super::{EffectId, EffectParams, ParamDescriptor, EFFECT_FILTER};

#[derive(Clone, Copy, Debug)]
struct BiquadCoefs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl BiquadCoefs {
    const fn passthrough() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct BiquadState {
    z1: f32,
    z2: f32,
}

impl BiquadState {
    const fn new() -> Self {
        Self { z1: 0.0, z2: 0.0 }
    }
    #[inline]
    fn process_sample(&mut self, x: f32, c: &BiquadCoefs) -> f32 {
        // Transposed Direct Form II — one mul-add per state.
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }
}

pub struct Filter {
    coefs: BiquadCoefs,
    state_l: BiquadState,
    state_r: BiquadState,
    /// Cache of the last (cutoff, q, mode, sample_rate) we computed
    /// coefficients for so we can skip the trig in the hot path.
    last_cutoff: f32,
    last_q: f32,
    last_mode: u8,
    last_sr: u32,
}

impl Filter {
    pub const DESCRIPTORS: &'static [ParamDescriptor] = &[
        ParamDescriptor {
            name: "cutoff_hz",
            min: 20.0,
            max: 20_000.0,
            default: 500.0,
        },
        ParamDescriptor {
            name: "resonance",
            min: 0.0,
            max: 1.0,
            default: 0.3,
        },
        ParamDescriptor {
            name: "mode",
            min: 0.0,
            max: 2.0,
            default: 0.0,
        },
    ];

    pub fn new() -> Self {
        Self {
            coefs: BiquadCoefs::passthrough(),
            state_l: BiquadState::new(),
            state_r: BiquadState::new(),
            last_cutoff: f32::NAN,
            last_q: f32::NAN,
            last_mode: 255,
            last_sr: 0,
        }
    }
}

impl Default for Filter {
    fn default() -> Self {
        Self::new()
    }
}

impl Filter {
    /// Map UI resonance ∈ [0,1] to a sensible Q range. 0 → 0.707
    /// (Butterworth); 1 → ~8 (rings). RBJ Q is "Q factor"; >0.707 is
    /// resonant.
    #[inline]
    fn resonance_to_q(res: f32) -> f32 {
        // exponential mapping → musical knob feel
        0.707 + res.clamp(0.0, 1.0) * 7.3
    }

    fn compute_coefs(cutoff_hz: f32, q: f32, mode: u8, sample_rate: u32) -> BiquadCoefs {
        // RBJ cookbook biquads. Frequencies clamped to a safe range
        // below Nyquist; Q clamped >0 to avoid division explosions.
        let sr = sample_rate as f32;
        let nyq = sr * 0.5;
        let f0 = cutoff_hz.clamp(20.0, nyq * 0.99);
        let q = q.max(0.1);
        let w0 = std::f32::consts::TAU * f0 / sr;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let (b0, b1, b2);
        match mode {
            1 => {
                // High-pass
                b0 = (1.0 + cos_w0) * 0.5;
                b1 = -(1.0 + cos_w0);
                b2 = (1.0 + cos_w0) * 0.5;
            }
            2 => {
                // Band-pass (constant skirt gain, peak = Q)
                b0 = sin_w0 * 0.5;
                b1 = 0.0;
                b2 = -sin_w0 * 0.5;
            }
            _ => {
                // Low-pass (default)
                b0 = (1.0 - cos_w0) * 0.5;
                b1 = 1.0 - cos_w0;
                b2 = (1.0 - cos_w0) * 0.5;
            }
        }
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        BiquadCoefs {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// Recompute coefficients if any of (cutoff, resonance, mode, sr)
    /// changed. **Audio-thread side** but trig only fires on parameter
    /// changes; the steady-state hot path is pure biquad multiplies.
    #[inline]
    fn refresh_coefs(&mut self, cutoff: f32, q: f32, mode: u8, sample_rate: u32) {
        // Bitwise equality is fine for f32 cache check — same source
        // means same bits.
        if self.last_cutoff.to_bits() == cutoff.to_bits()
            && self.last_q.to_bits() == q.to_bits()
            && self.last_mode == mode
            && self.last_sr == sample_rate
        {
            return;
        }
        self.coefs = Self::compute_coefs(cutoff, q, mode, sample_rate);
        self.last_cutoff = cutoff;
        self.last_q = q;
        self.last_mode = mode;
        self.last_sr = sample_rate;
    }
}

impl super::Effect for Filter {
    fn id(&self) -> EffectId {
        EFFECT_FILTER
    }
    fn name(&self) -> &'static str {
        "filter"
    }
    fn params(&self) -> &'static [ParamDescriptor] {
        Self::DESCRIPTORS
    }
    fn process(&mut self, buf: &mut [f32], params: &EffectParams, wet_dry: f32, sample_rate: u32) {
        let cutoff = params[0];
        let q = Self::resonance_to_q(params[1]);
        let mode = params[2].round().clamp(0.0, 2.0) as u8;
        self.refresh_coefs(cutoff, q, mode, sample_rate);
        let coefs = self.coefs;
        let dry = 1.0 - wet_dry;
        // Interleaved stereo: 2 samples per frame.
        let n_frames = buf.len() / 2;
        for f in 0..n_frames {
            let il = buf[f * 2];
            let ir = buf[f * 2 + 1];
            let ol = self.state_l.process_sample(il, &coefs);
            let or = self.state_r.process_sample(ir, &coefs);
            buf[f * 2] = dry * il + wet_dry * ol;
            buf[f * 2 + 1] = dry * ir + wet_dry * or;
        }
    }
    fn reset(&mut self) {
        self.state_l = BiquadState::new();
        self.state_r = BiquadState::new();
        self.last_cutoff = f32::NAN;
        self.last_q = f32::NAN;
        self.last_mode = 255;
        self.last_sr = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::super::Effect;
    use super::*;

    /// Lowpass at 1kHz cutoff attenuates 10kHz sine by ≥20dB
    /// (spec §7 acceptance).
    #[test]
    fn lowpass_attenuates_high_frequency() {
        let sr: u32 = 48_000;
        let mut f = Filter::new();
        // Generate 10kHz stereo sine, 4096 frames.
        let n = 4096;
        let mut buf = vec![0.0_f32; n * 2];
        let freq = 10_000.0_f32;
        for i in 0..n {
            let s = (std::f32::consts::TAU * freq * i as f32 / sr as f32).sin();
            buf[i * 2] = s;
            buf[i * 2 + 1] = s;
        }
        // cutoff=1000, resonance=0.0, mode=0 (LP)
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 1000.0;
        params[1] = 0.0;
        params[2] = 0.0;
        f.process(&mut buf, &params, 1.0, sr);
        // Skip first 256 frames (filter warm-up), measure RMS of tail.
        let tail_start = 256 * 2;
        let energy: f32 = buf[tail_start..].iter().map(|s| s * s).sum();
        let rms = (energy / (buf.len() - tail_start) as f32).sqrt();
        // Input RMS is 1/sqrt(2) ≈ 0.707. -20dB = 0.1× → expected
        // output RMS ≤ ~0.0707.
        assert!(
            rms < 0.0707,
            "lowpass should drop 10kHz by ≥20dB; got RMS {rms} (≈ {:.1} dB)",
            20.0 * rms.log10()
        );
    }

    /// Highpass at 1kHz cutoff attenuates 100Hz sine by ≥20dB.
    #[test]
    fn highpass_attenuates_low_frequency() {
        let sr: u32 = 48_000;
        let mut f = Filter::new();
        let n = 8192;
        let mut buf = vec![0.0_f32; n * 2];
        let freq = 100.0_f32;
        for i in 0..n {
            let s = (std::f32::consts::TAU * freq * i as f32 / sr as f32).sin();
            buf[i * 2] = s;
            buf[i * 2 + 1] = s;
        }
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 1000.0;
        params[1] = 0.0;
        params[2] = 1.0; // HP
        f.process(&mut buf, &params, 1.0, sr);
        let tail_start = 1024 * 2;
        let energy: f32 = buf[tail_start..].iter().map(|s| s * s).sum();
        let rms = (energy / (buf.len() - tail_start) as f32).sqrt();
        assert!(
            rms < 0.0707,
            "highpass should drop 100Hz by ≥20dB; got RMS {rms} (≈ {:.1} dB)",
            20.0 * rms.log10()
        );
    }

    #[test]
    fn passthrough_when_dry() {
        // wet_dry = 0 → output equals input regardless of params.
        let sr: u32 = 48_000;
        let mut f = Filter::new();
        let mut buf = vec![0.0_f32; 256];
        for (i, s) in buf.iter_mut().enumerate() {
            *s = (i as f32 * 0.01).sin();
        }
        let orig = buf.clone();
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 1000.0;
        params[1] = 0.5;
        params[2] = 0.0;
        f.process(&mut buf, &params, 0.0, sr);
        for (a, b) in orig.iter().zip(buf.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "dry filter should be passthrough: in={a} out={b}"
            );
        }
    }

    #[test]
    fn reset_clears_state() {
        let mut f = Filter::new();
        let mut params = [0.0_f32; super::super::MAX_PARAMS];
        params[0] = 500.0;
        params[1] = 0.8;
        params[2] = 0.0;
        // Run a transient through.
        let mut buf = vec![0.0_f32; 256];
        buf[0] = 1.0;
        f.process(&mut buf, &params, 1.0, 48_000);
        // After reset, feeding zeros gives zeros (filter has no
        // residual energy).
        f.reset();
        let mut zeros = vec![0.0_f32; 256];
        f.process(&mut zeros, &params, 1.0, 48_000);
        assert!(zeros.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn coef_cache_is_stable_when_params_constant() {
        let mut f = Filter::new();
        let params = [500.0, 0.3, 0.0, 0.0, 0.0, 0.0];
        let mut buf = vec![0.0_f32; 256];
        f.process(&mut buf, &params, 1.0, 48_000);
        let coefs_after_first = f.coefs;
        f.process(&mut buf, &params, 1.0, 48_000);
        // Cache hit → coefs untouched.
        assert_eq!(coefs_after_first.b0.to_bits(), f.coefs.b0.to_bits());
    }

    #[test]
    fn assert_no_alloc_filter_1024() {
        let mut f = Filter::new();
        let mut buf = [0.0_f32; 2048]; // 1024 stereo frames
        let params = [500.0, 0.3, 0.0, 0.0, 0.0, 0.0];
        assert_no_alloc::assert_no_alloc(|| {
            f.process(&mut buf, &params, 0.7, 48_000);
        });
    }
}
