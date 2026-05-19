//! Sidechain compressor — kick-driven ducking (issue #119).
//!
//! Industry-standard DJ ducking: one deck (the "trigger") owns the
//! kick; the other deck dips its volume under each kick hit so the
//! mix retains punch. Same architecture as Ableton's compressor in
//! `Mid Sidechain` mode minus the band-split filter (defer to v2).
//!
//! # DSP block diagram
//!
//! ```text
//!   trigger_deck_mono ──▶  envelope_follower(att, rel)  ──▶ env_db
//!                                                              │
//!   non_trigger_deck  ──▶  compressor(env_db, thr, ratio)  ──▶ * gain * 10^(makeup/20)
//! ```
//!
//! # Realtime safety
//!
//! All functions are `#[inline]` + scalar f32 — no allocation, no
//! locking, no syscalls. State is a single mutable f32 (the envelope
//! one-pole). Safe to call from the audio callback per ADR-004.
//!
//! # Conventions
//!
//! - All gains expressed in linear units (0..1+); dB conversions are
//!   confined to the public-facing helpers `db_to_linear` /
//!   `linear_to_db` for clarity.
//! - Attack / release times are measured from the 1 / e (≈37%) point —
//!   matches the textbook "tau" definition; users expecting "to-50%"
//!   times can mentally multiply by `ln(2) / 1 ≈ 0.693`.
//!
//! # What this module ISN'T (yet)
//!
//! - Band-split detection (kick is ~60-150 Hz; full-band envelope here
//!   for v0). Filed as P3 follow-up.
//! - Lookahead — no peek buffer.
//! - Soft-knee — hard-knee for now (cheaper, fine for ducking duties).

/// Audio-thread state for the sidechain. One mutable f32 envelope; the
/// rest are config parameters mirrored from `state::SidechainConfig`
/// via [`crate::audio::command::AudioCommandKind::SetSidechain`].
#[derive(Debug, Clone, Copy)]
pub struct SidechainState {
    pub enabled: bool,
    pub trigger_deck_is_a: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_gain_db: f32,
    /// One-pole envelope state (linear amplitude). Persists across
    /// audio callbacks so the attack/release curve stays continuous.
    pub envelope: f32,
}

impl Default for SidechainState {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_deck_is_a: true,
            threshold_db: -12.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 200.0,
            makeup_gain_db: 0.0,
            envelope: 0.0,
        }
    }
}

/// dB → linear amplitude. `+0 dB = 1.0`, `-6 dB ≈ 0.5`, `-∞ → 0`.
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    10f32.powf(db / 20.0)
}

/// Linear amplitude → dB. `0` clamps to `-∞` (returned as `f32::NEG_INFINITY`).
#[inline]
pub fn linear_to_db(lin: f32) -> f32 {
    if lin <= 0.0 {
        return f32::NEG_INFINITY;
    }
    20.0 * lin.log10()
}

/// One-pole coefficient for an attack/release time at the given
/// sample rate. Result is `exp(-1 / (time_s * sample_rate))` — the
/// classic envelope-follower constant. Clamped against degenerate
/// inputs (zero time → instant; non-finite → no-op).
#[inline]
pub fn time_coefficient(time_ms: f32, sample_rate: u32) -> f32 {
    if !time_ms.is_finite() || time_ms <= 0.0 || sample_rate == 0 {
        return 0.0;
    }
    let time_s = (time_ms / 1000.0).max(1e-6);
    (-1.0 / (time_s * sample_rate as f32)).exp()
}

/// Advance the envelope follower one sample. `input_abs` is the
/// instantaneous full-wave-rectified signal (`sample.abs()`). Returns
/// the new envelope amplitude. Attack coefficient applies when the
/// envelope is rising toward the input; release when it's falling.
#[inline]
pub fn envelope_step(envelope: f32, input_abs: f32, attack_coef: f32, release_coef: f32) -> f32 {
    let coef = if input_abs > envelope {
        attack_coef
    } else {
        release_coef
    };
    coef * envelope + (1.0 - coef) * input_abs
}

/// Compute the gain reduction (linear, ≤ 1.0) for a given envelope
/// value + compressor params. `threshold_db` is the level above which
/// reduction kicks in; `ratio` is `input_db_above_threshold : 1`. Hard
/// knee. Returns `1.0` when envelope is at or below threshold.
#[inline]
pub fn compressor_gain(envelope: f32, threshold_db: f32, ratio: f32) -> f32 {
    let env_db = linear_to_db(envelope);
    if !env_db.is_finite() {
        return 1.0;
    }
    let over = env_db - threshold_db;
    if over <= 0.0 {
        return 1.0;
    }
    let ratio_safe = ratio.max(1.0);
    let reduction_db = over - over / ratio_safe;
    db_to_linear(-reduction_db)
}

/// Process one sample pair (trigger + non-trigger). Returns
/// `(trigger_out, non_trigger_out)` — trigger pass-through, non-trigger
/// scaled by `gain_reduction * makeup`. Mutates `state.envelope`.
///
/// Caller is responsible for routing the correct deck channel to
/// `trigger` and `non_trigger`; the bool `state.trigger_deck_is_a`
/// only affects the upstream router (mixer), not this fn.
#[inline]
pub fn process_sample(
    trigger: f32,
    non_trigger: f32,
    state: &mut SidechainState,
    attack_coef: f32,
    release_coef: f32,
    makeup_lin: f32,
) -> (f32, f32) {
    if !state.enabled {
        return (trigger, non_trigger);
    }
    state.envelope = envelope_step(state.envelope, trigger.abs(), attack_coef, release_coef);
    let gain = compressor_gain(state.envelope, state.threshold_db, state.ratio);
    (trigger, non_trigger * gain * makeup_lin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_linear_roundtrip() {
        for db in [-60.0, -24.0, -12.0, -6.0, 0.0, 6.0, 12.0] {
            let lin = db_to_linear(db);
            let back = linear_to_db(lin);
            assert!((back - db).abs() < 0.001, "roundtrip {db} → {lin} → {back}");
        }
    }

    #[test]
    fn db_to_linear_handles_non_finite() {
        assert_eq!(db_to_linear(f32::NAN), 0.0);
        assert_eq!(db_to_linear(f32::INFINITY), 0.0);
    }

    #[test]
    fn linear_to_db_handles_zero_and_negative() {
        assert_eq!(linear_to_db(0.0), f32::NEG_INFINITY);
        assert_eq!(linear_to_db(-0.5), f32::NEG_INFINITY);
    }

    #[test]
    fn time_coefficient_within_unit_interval() {
        let c = time_coefficient(5.0, 48_000);
        assert!(c > 0.0 && c < 1.0, "coef={c}");
        let c200 = time_coefficient(200.0, 48_000);
        assert!(c200 > c, "200ms release > 5ms attack coef");
    }

    #[test]
    fn time_coefficient_handles_degenerate_inputs() {
        assert_eq!(time_coefficient(0.0, 48_000), 0.0);
        assert_eq!(time_coefficient(-5.0, 48_000), 0.0);
        assert_eq!(time_coefficient(5.0, 0), 0.0);
        assert_eq!(time_coefficient(f32::NAN, 48_000), 0.0);
    }

    #[test]
    fn envelope_rises_on_loud_input() {
        let a = time_coefficient(5.0, 48_000);
        let r = time_coefficient(200.0, 48_000);
        let mut env = 0.0;
        for _ in 0..(48_000 / 100) {
            env = envelope_step(env, 1.0, a, r);
        }
        assert!(env > 0.5, "envelope should rise quickly; got {env}");
    }

    #[test]
    fn envelope_falls_when_input_drops() {
        let a = time_coefficient(5.0, 48_000);
        let r = time_coefficient(200.0, 48_000);
        let mut env = 0.9;
        for _ in 0..48_000 {
            env = envelope_step(env, 0.0, a, r);
        }
        assert!(env < 0.01, "envelope should release toward 0; got {env}");
    }

    #[test]
    fn compressor_gain_unity_below_threshold() {
        let g = compressor_gain(db_to_linear(-20.0), -12.0, 4.0);
        assert!((g - 1.0).abs() < 1e-6);
    }

    #[test]
    fn compressor_gain_reduces_above_threshold_per_ratio() {
        // -12 dB threshold, 4:1 ratio. Input at -0 dB → over=12 dB →
        // reduction = 12 - 12/4 = 9 dB → linear ≈ 0.354.
        let g = compressor_gain(db_to_linear(0.0), -12.0, 4.0);
        let expected = db_to_linear(-9.0);
        assert!(
            (g - expected).abs() < 1e-3,
            "compressor 4:1 @ 0dB → expected {expected}, got {g}",
        );
    }

    #[test]
    fn compressor_gain_handles_ratio_less_than_one() {
        // Defensive: ratio < 1 silently clamps to 1 (no expansion).
        let g = compressor_gain(db_to_linear(0.0), -12.0, 0.5);
        assert!((g - 1.0).abs() < 1e-6);
    }

    #[test]
    fn process_sample_passthrough_when_disabled() {
        let mut s = SidechainState {
            enabled: false,
            ..Default::default()
        };
        let a = time_coefficient(5.0, 48_000);
        let r = time_coefficient(200.0, 48_000);
        let (t, n) = process_sample(0.5, 0.3, &mut s, a, r, 1.0);
        assert_eq!(t, 0.5);
        assert_eq!(n, 0.3);
        assert_eq!(s.envelope, 0.0, "envelope should not advance when disabled");
    }

    #[test]
    fn process_sample_ducks_non_trigger_when_enabled() {
        let mut s = SidechainState {
            enabled: true,
            envelope: db_to_linear(-3.0), // already above -12 dB threshold
            ..Default::default()
        };
        let a = time_coefficient(5.0, 48_000);
        let r = time_coefficient(200.0, 48_000);
        let (_t, n) = process_sample(1.0, 1.0, &mut s, a, r, 1.0);
        assert!(
            n < 1.0,
            "non-trigger should be ducked when env > thr; got {n}"
        );
    }

    #[test]
    fn process_sample_applies_makeup_gain() {
        let mut s = SidechainState {
            enabled: true,
            envelope: 0.0, // below threshold → gain=1
            makeup_gain_db: 6.0,
            ..Default::default()
        };
        let a = time_coefficient(5.0, 48_000);
        let r = time_coefficient(200.0, 48_000);
        let makeup = db_to_linear(s.makeup_gain_db);
        let (_t, n) = process_sample(0.0, 0.5, &mut s, a, r, makeup);
        // +6 dB makeup ≈ ×2 → 1.0
        assert!(
            (n - 1.0).abs() < 0.01,
            "makeup should ~double signal; got {n}"
        );
    }
}
