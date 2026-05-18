//! Master-bus soft-clip limiter.
//!
//! Sits on the final master sum **before** the cpal output and **before**
//! the recorder tee, so the live mix + the saved `master.wav` are both
//! protected against clipping when both decks are loud + effects (echo,
//! reverb) are active.
//!
//! # Algorithm
//!
//! Two-stage chain — fast envelope-follower-driven gain reduction
//! followed by a smooth tanh ceiling. The intent is **transparent
//! protection**, not creative compression: the limiter should be inaudible
//! until the master bus is hot, at which point it shaves the worst
//! transients and rounds off anything that still tries to escape ±1.0.
//!
//! ```text
//!     in ──▶ |x|  ┌── attack/release ──▶ envelope ──┐
//!                 │                                  │
//!                 ▼                                  ▼
//!              one-pole       gain = min(1, thr/env) (linear)
//!                 │                                  │
//!                 │                            x · gain
//!                 │                                  │
//!                 │                                  ▼
//!                 │                       tanh(s · m) / tanh(m)
//!                 │                                  │
//!                 └──────────────────────────────────▶ out
//! ```
//!
//! * **Envelope follower** — peak-absolute, asymmetric one-pole. Fast
//!   attack (0.5 ms by default) catches transients; slower release
//!   (50 ms) keeps the gain reduction from "pumping" on subsequent
//!   samples.
//! * **Gain reduction** — if the envelope sits above the linear threshold
//!   we multiply by `threshold / envelope`, so the envelope-shaped peak
//!   is brought *exactly* to threshold. Below threshold the gain is 1.0.
//! * **Soft-clip ceiling** — classic hyperbolic saturation:
//!   `y = c · tanh(s / c)` where `c` is the linear threshold. For
//!   small `s` this is approximately linear (`tanh(x) ≈ x` near 0,
//!   so `y ≈ s` — transparent below threshold). For large `|s|` it
//!   asymptotes toward `±c`, smoothly rounding off any transient that
//!   slipped past the envelope follower's attack window.
//! * **Hard clamp** — final `clamp(-1.0, 1.0)` so a denormal or numeric
//!   weirdness can't ever escape past unity to the device / file. Pure
//!   defensive code; in normal operation the tanh stage already keeps
//!   the output strictly inside `±c ≤ ±1.0`.
//!
//! ## Bypass
//!
//! When `enabled == false`, [`MasterLimiter::process`] is a no-op. The
//! envelope follower state is **frozen** (not reset) so re-enabling the
//! limiter during a hot section doesn't ping the envelope back to zero
//! and produce an audible burst.
//!
//! ## Defaults
//!
//! | Param         | Value         | Why                                          |
//! |---------------|---------------|----------------------------------------------|
//! | `threshold_db`| -0.5 dB       | Tucks the master ~0.5 dB under unity.        |
//! | `attack_ms`   | 0.5 ms        | Catch transients (a 1-sample spike at 48 kHz |
//! |               |               | is ≈ 21 µs — 0.5 ms covers ~24 samples).     |
//! | `release_ms`  | 50 ms         | Smooth recovery, no pumping on kick patterns |
//! | `enabled`     | `true`        | Limiter ON by default — safety-first.        |
//!
//! (A `makeup` softness parameter is reserved internally for future
//! shape-curve tuning but is currently a no-op constant.)
//!
//! ## ADR-004 compliance
//!
//! * `process` mutates only stack-resident state + the caller's slice.
//!   No allocation. No locks. Verified by `assert_no_alloc` in tests.
//! * No `unsafe`. The tanh, ln, and exp calls go through Rust's `f32`
//!   intrinsics which are pure.

/// Default threshold in dB. `-0.5 dB` linear ≈ `0.94406`.
pub const DEFAULT_THRESHOLD_DB: f32 = -0.5;

/// Default attack time-constant in milliseconds.
pub const DEFAULT_ATTACK_MS: f32 = 0.5;

/// Default release time-constant in milliseconds.
pub const DEFAULT_RELEASE_MS: f32 = 50.0;

/// Minimum threshold expressible via the UI / event API, in dB. Hard
/// limit below which the limiter would erase the mix.
pub const MIN_THRESHOLD_DB: f32 = -24.0;

/// Maximum threshold expressible via the UI / event API, in dB.
pub const MAX_THRESHOLD_DB: f32 = 0.0;

/// Floor for the envelope follower. Below this the division
/// `threshold / envelope` collapses to gain = 1.0 (which we'd already
/// pick anyway), so we just early-exit the heavy math.
const ENV_FLOOR: f32 = 1.0e-9;

/// Master-bus soft-clip limiter.
///
/// Internal state is `Copy`-shaped + audio-thread safe — `process` takes
/// `&mut self` so the envelope follower can carry over between
/// callbacks. Construct once at audio-thread start; bypass via
/// [`MasterLimiter::set_enabled`] when you want to skip processing.
#[derive(Clone, Copy, Debug)]
pub struct MasterLimiter {
    /// Linear threshold (≤ 1.0). Updated whenever `threshold_db` is
    /// changed; cached so the hot path doesn't recompute `10^(db/20)`.
    threshold_linear: f32,
    /// Attack one-pole coefficient (≈ `1 - exp(-1 / (attack_samples))`).
    /// Re-derived in `set_sample_rate` / `new`.
    attack_coeff: f32,
    /// Release one-pole coefficient.
    release_coeff: f32,
    /// Sample rate the attack/release coefficients were computed for.
    /// `process` re-derives them lazily if the caller hands in a
    /// different `sample_rate`.
    cached_sample_rate: u32,
    /// Configured attack time in milliseconds. Re-used when the sample
    /// rate changes.
    attack_ms: f32,
    /// Configured release time in milliseconds.
    release_ms: f32,
    /// Envelope follower's running peak estimate. Smoothed by the
    /// one-pole filter every sample.
    envelope: f32,
    /// Bypass switch. When false, `process` returns immediately.
    enabled: bool,
}

impl MasterLimiter {
    /// Construct with default threshold/attack/release/makeup +
    /// `enabled = true`.
    pub fn new(sample_rate: u32) -> Self {
        let mut m = Self {
            threshold_linear: db_to_linear(DEFAULT_THRESHOLD_DB),
            attack_coeff: 0.0,
            release_coeff: 0.0,
            cached_sample_rate: 0,
            attack_ms: DEFAULT_ATTACK_MS,
            release_ms: DEFAULT_RELEASE_MS,
            envelope: 0.0,
            enabled: true,
        };
        m.recompute_coeffs(sample_rate);
        m
    }

    /// Enable / disable bypass. When disabling, the envelope is
    /// **frozen** (not reset) so a quick toggle doesn't audibly pop.
    #[inline]
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Is the limiter currently active (not bypassed)?
    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Set the threshold in dB. The caller is expected to have already
    /// clamped via [`clamp_threshold_db`], but we re-clamp defensively
    /// so a malformed event can't push us past the ceiling.
    #[inline]
    pub fn set_threshold_db(&mut self, db: f32) {
        let clamped = clamp_threshold_db(db);
        self.threshold_linear = db_to_linear(clamped);
    }

    /// Current linear threshold (for tests + introspection).
    #[inline]
    pub fn threshold_linear(&self) -> f32 {
        self.threshold_linear
    }

    /// Current envelope estimate (for tests).
    #[inline]
    pub fn envelope(&self) -> f32 {
        self.envelope
    }

    /// In-place processing. **Audio-thread side — alloc-free.**
    ///
    /// `buf` is a slice of mono master samples (one f32 per frame). The
    /// limiter applies envelope-follower gain reduction + a tanh
    /// soft-clip and re-writes `buf` in place.
    ///
    /// When `enabled == false` this returns immediately — zero CPU.
    #[inline]
    pub fn process(&mut self, buf: &mut [f32], sample_rate: u32) {
        if !self.enabled {
            return;
        }
        if sample_rate != self.cached_sample_rate {
            self.recompute_coeffs(sample_rate);
        }
        let thr = self.threshold_linear;
        let a = self.attack_coeff;
        let r = self.release_coeff;
        let mut env = self.envelope;

        // Effective ceiling for the soft-clip stage. The threshold is
        // clamped to ENV_FLOOR so the `s / c` divide can't blow up
        // under degenerate config (threshold ≈ 0).
        let c = thr.max(ENV_FLOOR);

        for s in buf.iter_mut() {
            // Envelope follower — peak absolute, asymmetric one-pole.
            let abs = s.abs();
            let coeff = if abs > env { a } else { r };
            // env += coeff * (abs - env)
            env += coeff * (abs - env);

            // Gain reduction (only when envelope > threshold).
            let gain = if env > c { thr / env } else { 1.0 };
            let scaled = *s * gain;

            // Soft-clip via hyperbolic saturation.
            //   y = c · tanh(s / c)
            // For small `|s|`, `tanh(x) ≈ x`, so `y ≈ s` (transparent
            // pass-through below threshold — exactly what we want for
            // an inaudible "protection" limiter). For large `|s|`,
            // `y → ±c`, smoothly rounding off any transient that
            // slipped past the envelope follower's attack window.
            // The hard clamp guards against denormals / numerics.
            let shaped = c * (scaled / c).tanh();
            *s = shaped.clamp(-1.0, 1.0);
        }

        self.envelope = env;
    }

    /// Re-derive the attack/release one-pole coefficients for a new
    /// sample rate. Called lazily inside `process` when the host swaps
    /// device rates.
    fn recompute_coeffs(&mut self, sample_rate: u32) {
        let sr = sample_rate.max(1) as f32;
        // One-pole time constant: coeff = 1 - exp(-1 / (tau · sr))
        // where tau is the time-constant in seconds. A 1-tau step
        // reaches ~63% of the target, which is the standard
        // envelope-follower convention.
        let to_coeff = |ms: f32| -> f32 {
            let tau_s = (ms / 1000.0).max(1.0e-6);
            1.0 - (-1.0 / (tau_s * sr)).exp()
        };
        self.attack_coeff = to_coeff(self.attack_ms);
        self.release_coeff = to_coeff(self.release_ms);
        self.cached_sample_rate = sample_rate;
    }
}

/// dB → linear amplitude. Inlined helper so callers don't pull in any
/// of the `audio::mixer` private helpers.
#[inline]
fn db_to_linear(db: f32) -> f32 {
    (db * (std::f32::consts::LN_10 / 20.0)).exp()
}

/// Clamp a threshold dB value to the supported range. Used both in the
/// reducer (control side) and inside the limiter (defensive).
#[inline]
pub fn clamp_threshold_db(db: f32) -> f32 {
    if db.is_finite() {
        db.clamp(MIN_THRESHOLD_DB, MAX_THRESHOLD_DB)
    } else {
        DEFAULT_THRESHOLD_DB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// Feeding a 1.5-amplitude sine through the limiter keeps the
    /// output strictly inside ±1.0. This is the "no clipping" guarantee
    /// — the live mix + the recorded `master.wav` both rely on it.
    #[test]
    fn limiter_attenuates_above_threshold() {
        let mut lim = MasterLimiter::new(SR);
        let mut buf = [0.0_f32; 2048];
        // 440 Hz at 1.5 amplitude — way over unity.
        let freq = 440.0_f32;
        let dphase = std::f32::consts::TAU * freq / SR as f32;
        let mut phase = 0.0_f32;
        for s in buf.iter_mut() {
            *s = phase.sin() * 1.5;
            phase += dphase;
        }
        lim.process(&mut buf, SR);
        for (i, s) in buf.iter().enumerate() {
            assert!(
                s.abs() <= 1.0 + 1e-6,
                "sample {i} = {s} escaped ±1.0 after limiter",
            );
        }
        // And after the attack window we should sit near the soft-clip
        // ceiling (≈ `threshold · tanh(1) ≈ 0.72` for default config)
        // — proves we're not over-attenuating.
        let tail_peak = buf[1024..].iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
        assert!(
            tail_peak > 0.6,
            "expected limiter to leave a hot signal (≥ 0.6), got peak {tail_peak}",
        );
    }

    /// Disabled limiter must be a strict no-op: input == output.
    #[test]
    fn limiter_bypass_when_disabled() {
        let mut lim = MasterLimiter::new(SR);
        lim.set_enabled(false);
        let input: Vec<f32> = (0..512).map(|i| (i as f32) * 0.01).collect();
        let mut buf = input.clone();
        lim.process(&mut buf, SR);
        for (i, (a, b)) in input.iter().zip(buf.iter()).enumerate() {
            assert!(
                (a - b).abs() < f32::EPSILON,
                "sample {i}: {a} vs {b}; bypass mode should be passthrough",
            );
        }
        assert!(!lim.enabled());
    }

    /// Driving the limiter with a hot tone for one block, then a quiet
    /// one, should let the gain recover to ~1.0 within the release
    /// window. We bound at 250 ms (≈ 5× release time) to give the
    /// asymmetric one-pole enough room.
    #[test]
    fn limiter_release_recovers() {
        let mut lim = MasterLimiter::new(SR);
        // Hot phase: 100 ms of saturating signal.
        let hot_frames = (SR / 10) as usize; // 100 ms
        let mut hot = vec![1.5_f32; hot_frames];
        lim.process(&mut hot, SR);
        assert!(
            lim.envelope() > 1.0,
            "envelope should be high after hot signal, got {}",
            lim.envelope(),
        );
        // Quiet phase: 250 ms of tiny signal. The envelope should
        // decay back into the < threshold range so the gain-reduction
        // branch goes idle.
        let quiet_frames = (SR / 4) as usize; // 250 ms
        let mut quiet = vec![0.01_f32; quiet_frames];
        lim.process(&mut quiet, SR);
        let env = lim.envelope();
        assert!(
            env < lim.threshold_linear(),
            "envelope ({env}) did not recover below threshold ({}) after release window",
            lim.threshold_linear(),
        );
        // And the trailing samples should be ~equal to input (no
        // attenuation) since gain == 1.0 now.
        let last = quiet.last().copied().unwrap();
        assert!(
            (last - 0.01).abs() < 1e-3,
            "trailing quiet sample should be near input value, got {last}",
        );
    }

    /// ADR-004: `process` MUST be alloc-free on the audio thread. This
    /// catches a regression of someone adding a `Vec` push or similar.
    #[test]
    fn limiter_alloc_free() {
        let mut lim = MasterLimiter::new(SR);
        let mut buf = [0.5_f32; 1024];
        // Prime — first call may settle internals (it doesn't today,
        // but be conservative).
        lim.process(&mut buf, SR);
        assert_no_alloc::assert_no_alloc(|| {
            lim.process(&mut buf, SR);
        });
    }

    /// Lowering the threshold should pin the output closer to the new
    /// (lower) ceiling. Reproduces the audio-thread effect of an
    /// inbound `SetMasterLimiterThreshold` event.
    #[test]
    fn limiter_threshold_change_via_event() {
        let mut lim = MasterLimiter::new(SR);
        lim.set_threshold_db(-12.0); // far below default
        let new_thr = lim.threshold_linear();
        // -12 dB linear ≈ 0.2512.
        assert!(
            (new_thr - 10_f32.powf(-12.0 / 20.0)).abs() < 1e-4,
            "threshold_linear after -12 dB event should be ≈ 0.2512, got {new_thr}",
        );
        // Push a hot signal — the trailing peak should now sit near
        // the new (much lower) threshold, not the default ≈ 0.944.
        let mut buf = vec![1.0_f32; 4096];
        lim.process(&mut buf, SR);
        let tail_peak = buf[2048..].iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
        assert!(
            tail_peak < 0.5,
            "expected tail peak under -12 dB threshold ceiling, got {tail_peak}",
        );
    }

    /// Defensive: an out-of-range threshold dB is clamped to the
    /// configured [`MIN_THRESHOLD_DB`, `MAX_THRESHOLD_DB`] window so a
    /// bad event payload can never erase the master bus.
    #[test]
    fn limiter_threshold_clamps_to_safe_window() {
        let mut lim = MasterLimiter::new(SR);
        lim.set_threshold_db(-100.0); // way below MIN
        assert!(
            (lim.threshold_linear() - db_to_linear(MIN_THRESHOLD_DB)).abs() < 1e-6,
            "threshold below MIN_THRESHOLD_DB must clamp to MIN",
        );
        lim.set_threshold_db(20.0); // above MAX
        assert!(
            (lim.threshold_linear() - db_to_linear(MAX_THRESHOLD_DB)).abs() < 1e-6,
            "threshold above MAX_THRESHOLD_DB must clamp to MAX",
        );
        // Non-finite → fall back to default, not propagate NaN.
        lim.set_threshold_db(f32::NAN);
        assert!(
            (lim.threshold_linear() - db_to_linear(DEFAULT_THRESHOLD_DB)).abs() < 1e-6,
            "NaN threshold should fall back to default",
        );
    }

    /// Bypass-then-resume must NOT reset the envelope (otherwise a UI
    /// toggle during a hot section would audibly pop). Disabled
    /// process() is a no-op; envelope stays put.
    #[test]
    fn limiter_disabled_freezes_envelope() {
        let mut lim = MasterLimiter::new(SR);
        let mut hot = vec![1.5_f32; 2048];
        lim.process(&mut hot, SR);
        let env_before = lim.envelope();
        assert!(env_before > 0.5);
        lim.set_enabled(false);
        let mut buf = [0.0_f32; 1024];
        lim.process(&mut buf, SR); // no-op
        assert!(
            (lim.envelope() - env_before).abs() < f32::EPSILON,
            "disabled process() must not mutate envelope state",
        );
    }

    /// The limiter's transparency goal: when the input is already
    /// safely under threshold, output should be very close to input.
    /// (Soft-clip's makeup curve introduces a tiny non-linearity but
    /// well under 1% at this level.)
    #[test]
    fn limiter_transparent_below_threshold() {
        let mut lim = MasterLimiter::new(SR);
        // 0.3 amplitude — well under -0.5 dB.
        let input: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
        let mut buf = input.clone();
        lim.process(&mut buf, SR);
        for (i, (a, b)) in input.iter().zip(buf.iter()).enumerate() {
            assert!(
                (a - b).abs() < 0.02,
                "sample {i}: limiter altered below-threshold signal too much: {a} -> {b}",
            );
        }
    }
}

#[cfg(test)]
mod perf {
    use super::*;
    use std::time::Instant;

    /// Worst-case latency probe for a 1024-frame process call. The
    /// audio thread's budget is ≈ 1ms per render at 48 kHz / 1024
    /// frames (= 21.3 ms wall-clock window). A solo limiter pass
    /// should be well under that.
    #[test]
    fn limiter_1024_frame_latency_probe() {
        let mut lim = MasterLimiter::new(48_000);
        let mut buf = [0.5_f32; 1024];
        // Warm-up.
        for _ in 0..32 {
            lim.process(&mut buf, 48_000);
        }
        let mut worst_ns: u128 = 0;
        for _ in 0..1000 {
            let t = Instant::now();
            lim.process(&mut buf, 48_000);
            let ns = t.elapsed().as_nanos();
            if ns > worst_ns {
                worst_ns = ns;
            }
        }
        eprintln!(
            "limiter_worst_case_1024frames_ns={worst_ns} (~{:.2}µs)",
            worst_ns as f64 / 1000.0
        );
        // 500 µs ceiling — generous; observed worst case is typically
        // ≤ 20 µs even in debug builds.
        assert!(
            worst_ns < 500_000,
            "limiter process took {worst_ns} ns for 1024 frames — over 500 µs budget",
        );
    }
}
