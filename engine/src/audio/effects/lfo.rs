//! LFO (Low Frequency Oscillator) modulation source for effect params.
//!
//! Pro-DJ effect units (Pioneer RMX-1000, Korg KAOSS, etc.) auto-modulate
//! a chosen effect parameter on a beat-synced clock — e.g. sweep a
//! filter cutoff in time with the bar, gate the duty in 1/16 pulses.
//!
//! Per-slot LFO design:
//!   * One LFO per [`super::FxBank`] slot. Targets exactly one param.
//!   * Beat-synced rate via the slot's shared [`SharedClock`] master BPM.
//!   * Pure-fn `value(sample, sr, bpm)` returns the modulation source in
//!     `[-1.0, +1.0]` — multiplied by `depth` × the param's descriptor
//!     range (or octaves, for filter cutoff) by the mixer before
//!     `process()` runs.
//!
//! Hard rules (ADR-004 §audio-thread, ADR-006 §effects):
//!   * No allocation. The struct is `Copy + Send + Sync + 'static` so it
//!     ships in a single SPSC ring slot as an `AudioCommand` payload.
//!   * No `unsafe`.
//!   * No locks.
//!   * Pure deterministic function of (frame, sr, bpm, shape, rate_div,
//!     depth, target_param). The audio thread does not own per-LFO
//!     phase — frame index is the phase.
//!
//! Param target convention:
//!   * `target_param` is the descriptor-index of the param being modulated
//!     (same numbering as `AudioCommandKind::EffectParam.param_id`).
//!   * Modulation mode is param-aware: the [`crate::audio::effects`] mixer
//!     integration treats filter `cutoff_hz` as **octaves** (multiplicative)
//!     so a swept sine sounds musically linear; other params are additive
//!     scaled by the descriptor's `(max - min)` range. See
//!     [`apply_lfo_to_params`] for the dispatch.

use crate::audio::clock::SharedClock;
use serde::{Deserialize, Serialize};

use super::EffectParams;

/// LFO waveform shapes. The discriminant is wire-stable across versions
/// — never renumber existing variants. Stored as a `u8` so it fits
/// inside an `AudioCommand` slot with no padding waste.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Shape {
    Sine = 0,
    Saw = 1,
    Square = 2,
    Triangle = 3,
}

impl Shape {
    /// Decode from a byte for wire / event safety. Unknown values fall
    /// back to `Sine` — defensive, mirrors `ClockSource::from_byte`.
    pub fn from_byte(b: u8) -> Self {
        match b {
            1 => Shape::Saw,
            2 => Shape::Square,
            3 => Shape::Triangle,
            _ => Shape::Sine,
        }
    }
    /// Stable kebab-case wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Shape::Sine => "sine",
            Shape::Saw => "saw",
            Shape::Square => "square",
            Shape::Triangle => "triangle",
        }
    }
}

/// LFO rate divisions. The period of one full LFO cycle equals
/// `Bar * 4 beats / bpm` for `Bar`, `1 beat / bpm` for `Beat`, etc.
///
/// Stored as a `u8` for compactness inside `AudioCommand`. The
/// discriminants are wire-stable.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RateDiv {
    Bar = 0,
    Beat = 1,
    Half = 2,
    Quarter = 3,
    Eighth = 4,
    Sixteenth = 5,
}

impl RateDiv {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => RateDiv::Bar,
            1 => RateDiv::Beat,
            2 => RateDiv::Half,
            3 => RateDiv::Quarter,
            4 => RateDiv::Eighth,
            5 => RateDiv::Sixteenth,
            _ => RateDiv::Quarter,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            RateDiv::Bar => "bar",
            RateDiv::Beat => "beat",
            RateDiv::Half => "1/2",
            RateDiv::Quarter => "1/4",
            RateDiv::Eighth => "1/8",
            RateDiv::Sixteenth => "1/16",
        }
    }

    /// Beats-per-LFO-cycle. Assumes 4/4. A `Bar` = 4 beats; `Beat` = 1;
    /// `Half` = 1/2 of a beat; `1/4` = a quarter of a beat; etc. This is
    /// the **musical** beat-fraction used to derive the period in seconds.
    #[inline]
    pub const fn beats_per_cycle(self) -> f32 {
        match self {
            RateDiv::Bar => 4.0,
            RateDiv::Beat => 1.0,
            RateDiv::Half => 0.5,
            RateDiv::Quarter => 0.25,
            RateDiv::Eighth => 0.125,
            RateDiv::Sixteenth => 0.0625,
        }
    }
}

/// One LFO configuration. POD — `Copy + Send + Sync + 'static` so it
/// rides the `AudioCommand` ring without allocation.
///
/// Field sizes: 1+1+4+1 = 7 bytes (plus padding) — well inside the
/// 64-byte `AudioCommand` ceiling.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct LfoConfig {
    pub shape: Shape,
    pub rate_div: RateDiv,
    /// Modulation depth in `[0.0, 1.0]`. 0 = LFO contributes nothing;
    /// 1 = full descriptor-range swing (or ±2 octaves for filter cutoff).
    pub depth: f32,
    /// Descriptor index of the param to modulate. Out-of-range values
    /// are silently ignored by `apply_lfo_to_params`.
    pub target_param: u8,
}

impl LfoConfig {
    /// Sensible default — a quarter-beat triangle on param 0 with full
    /// depth. Kept here so `Default` is meaningful for tests / UI.
    pub const fn new(shape: Shape, rate_div: RateDiv, depth: f32, target_param: u8) -> Self {
        Self {
            shape,
            rate_div,
            depth,
            target_param,
        }
    }
}

impl Default for LfoConfig {
    fn default() -> Self {
        Self::new(Shape::Sine, RateDiv::Quarter, 1.0, 0)
    }
}

/// Pure-fn LFO evaluator. The "state" is the absolute sample frame —
/// no phase accumulator needed, no per-buffer reset behaviour required.
/// Audio-thread friendly: zero allocation, branch-only on `shape`.
#[derive(Debug, Clone, Copy)]
pub struct Lfo {
    pub config: LfoConfig,
}

impl Lfo {
    #[inline]
    pub const fn new(config: LfoConfig) -> Self {
        Self { config }
    }

    /// Compute the LFO period in **samples** given (bpm, sr). Returns
    /// `None` for a degenerate setup (bpm <= 0 or period < 1 sample).
    /// Pure helper exposed for tests.
    #[inline]
    pub fn period_frames(rate_div: RateDiv, bpm: f32, sample_rate: u32) -> Option<u64> {
        if !bpm.is_finite() || bpm <= 0.0 {
            return None;
        }
        let beat_sec = 60.0 / bpm;
        let cycle_sec = beat_sec * rate_div.beats_per_cycle();
        let frames = (cycle_sec * sample_rate as f32) as u64;
        if frames == 0 {
            None
        } else {
            Some(frames)
        }
    }

    /// Mod-source value in `[-1.0, +1.0]` at the given absolute frame.
    ///
    /// Phase convention: at frame 0, all shapes start at 0 except saw
    /// (starts at -1, ramps to +1) so the math matches conventional DSP
    /// references and the test fixtures are unambiguous.
    ///
    /// Returns `0.0` when bpm <= 0 or the computed period is < 1 frame
    /// — passthrough behaviour matching `Gate::open_at`'s degenerate
    /// fallback (no modulation, no panic).
    #[inline]
    pub fn value(&self, sample_count: u64, sample_rate: u32, bpm: f32) -> f32 {
        let period = match Self::period_frames(self.config.rate_div, bpm, sample_rate) {
            Some(p) => p,
            None => return 0.0,
        };
        // Normalised phase in [0.0, 1.0).
        let phase = (sample_count % period) as f32 / period as f32;
        match self.config.shape {
            Shape::Sine => (std::f32::consts::TAU * phase).sin(),
            Shape::Saw => {
                // Rising saw: -1 at phase 0, +1 at phase 1.
                phase * 2.0 - 1.0
            }
            Shape::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Shape::Triangle => {
                // 0 at phase 0, +1 at 0.25, 0 at 0.5, -1 at 0.75, 0 at 1.
                // Closed-form: 1 - |4·(phase - 0.25 floor adjustment)| via
                // a piecewise linear over [0, 1) with peaks at ±1.
                let p4 = phase * 4.0;
                if phase < 0.25 {
                    p4
                } else if phase < 0.75 {
                    2.0 - p4
                } else {
                    p4 - 4.0
                }
            }
        }
    }
}

/// Mixer-side helper: given a slot's static `params` table and an LFO,
/// produce a **modulated** copy of the params for one `process()` call.
///
/// The audio-thread mixer calls this each render block, then hands the
/// returned `EffectParams` array to the effect's `process()`. No
/// allocation — `EffectParams` is `[f32; MAX_PARAMS]` (Copy POD).
///
/// Modulation rules per param index — chosen to be musically intuitive:
///   * Filter cutoff (`effect_id=EFFECT_FILTER`, param 0): **multiplicative**
///     in octaves. `cutoff' = cutoff * 2^(lfo_value * depth * 2)` so
///     `depth=1.0` swings ±2 octaves around the set point.
///   * Echo `time_ms` (effect 2, param 0): multiplicative in halvings.
///     `t' = t * 2^(lfo_value * depth)` so depth=1 swings ±1 octave
///     (×0.5 to ×2) — a classic dub-style tape-warble shape.
///   * Reverb `room_size` (effect 3, param 0): additive, scaled by the
///     descriptor's `(max-min)` × `depth` × 0.5 (half-range each side).
///   * Gate `duty` (effect 4, param 1): additive, depth × 0.5 around
///     the set point, clamped to the descriptor range.
///   * Everything else (mode selectors, feedback, tone, width, period_div):
///     additive depth × `(max-min) × 0.5`, clamped.
///
/// Out-of-range `target_param` is silently a passthrough.
#[inline]
pub fn apply_lfo_to_params(
    base: &EffectParams,
    lfo: &Lfo,
    effect_id: super::EffectId,
    sample_count: u64,
    sample_rate: u32,
    bpm: f32,
) -> EffectParams {
    let mut out = *base;
    let idx = lfo.config.target_param as usize;
    if idx >= super::MAX_PARAMS {
        return out;
    }
    let descs = super::descriptors(effect_id);
    let desc = match descs.get(idx) {
        Some(d) => d,
        None => return out,
    };
    let depth = lfo.config.depth.clamp(0.0, 1.0);
    if depth == 0.0 {
        return out;
    }
    let value = lfo.value(sample_count, sample_rate, bpm);
    let modulated = match (effect_id, idx) {
        // Filter cutoff — multiplicative in octaves (±2 octaves at depth=1).
        (super::EFFECT_FILTER, 0) => base[0] * 2f32.powf(value * depth * 2.0),
        // Echo time_ms — multiplicative in halvings (±1 octave at depth=1).
        (super::EFFECT_ECHO, 0) => base[0] * 2f32.powf(value * depth),
        // Everything else — additive, scaled by half the descriptor range.
        _ => {
            let range = desc.max - desc.min;
            base[idx] + value * depth * range * 0.5
        }
    };
    out[idx] = desc.clamp(modulated);
    out
}

/// Convenience wrapper — take a fresh BPM read off the [`SharedClock`]
/// and call [`Lfo::value`]. Used by the mixer integration so the call
/// site is one line.
#[inline]
pub fn lfo_value_from_clock(lfo: &Lfo, clock: &SharedClock, sample_rate: u32) -> f32 {
    let bpm = clock.master_bpm();
    let frame = clock.frame();
    lfo.value(frame, sample_rate, bpm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::effects::{EFFECT_ECHO, EFFECT_FILTER, EFFECT_GATE, EFFECT_REVERB};

    /// Sine LFO at 1 Hz returns 0 at frame 0 and 0 at the half-period
    /// (sin(π)≈0), +1 at 1/4 period (sin(π/2)=1). We pick a `Bar`
    /// rate_div with bpm tuned so cycle = 1 Hz: cycle_sec = 4 beats /
    /// bpm = 1 s → bpm = 240.
    #[test]
    fn sine_one_hz_period_matches_spec() {
        let sr: u32 = 48_000;
        let bpm = 240.0; // 4 beats / 240 BPM × 60s/min = 1 s cycle
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Bar, 1.0, 0));
        // At frame 0: phase 0 → sin(0) = 0.
        assert!(lfo.value(0, sr, bpm).abs() < 1e-3);
        // At 1/4 period (12000 frames): sin(π/2) = 1.
        assert!((lfo.value(12_000, sr, bpm) - 1.0).abs() < 1e-3);
        // At 1/2 period (24000 frames): sin(π) ≈ 0.
        assert!(lfo.value(24_000, sr, bpm).abs() < 1e-3);
        // At 3/4 period (36000 frames): sin(3π/2) = -1.
        assert!((lfo.value(36_000, sr, bpm) + 1.0).abs() < 1e-3);
        // Full period wraps back to 0.
        assert!(lfo.value(48_000, sr, bpm).abs() < 1e-3);
    }

    /// Saw monotonically rises from -1 to +1 over one period, then
    /// resets on the period boundary.
    #[test]
    fn saw_monotonic_then_resets() {
        let sr: u32 = 48_000;
        let bpm = 120.0;
        let lfo = Lfo::new(LfoConfig::new(Shape::Saw, RateDiv::Beat, 1.0, 0));
        let period = Lfo::period_frames(RateDiv::Beat, bpm, sr).unwrap();
        // Beat at 120 BPM = 0.5s = 24000 frames.
        assert_eq!(period, 24_000);
        // Frame 0 → -1.
        assert!((lfo.value(0, sr, bpm) + 1.0).abs() < 1e-3);
        // Half-period → 0.
        assert!(lfo.value(period / 2, sr, bpm).abs() < 1e-3);
        // Just before period boundary → close to +1.
        let almost = lfo.value(period - 1, sr, bpm);
        assert!(
            almost > 0.99,
            "near-end-of-period saw should approach +1; got {almost}"
        );
        // Period boundary → wraps to -1.
        assert!((lfo.value(period, sr, bpm) + 1.0).abs() < 1e-3);
        // Strictly monotonic between two interior samples.
        let earlier = lfo.value(1000, sr, bpm);
        let later = lfo.value(5000, sr, bpm);
        assert!(later > earlier, "saw should increase: {earlier} -> {later}");
    }

    /// Square alternates ±1 with no intermediate values.
    #[test]
    fn square_alternates_plus_minus_one() {
        let sr: u32 = 48_000;
        let bpm = 120.0;
        let lfo = Lfo::new(LfoConfig::new(Shape::Square, RateDiv::Beat, 1.0, 0));
        let period = Lfo::period_frames(RateDiv::Beat, bpm, sr).unwrap();
        // First half = +1.
        for f in [0u64, 100, period / 2 - 1] {
            assert!(
                (lfo.value(f, sr, bpm) - 1.0).abs() < 1e-9,
                "square first half should be +1 at frame {f}",
            );
        }
        // Second half = -1.
        for f in [period / 2, period / 2 + 100, period - 1] {
            assert!(
                (lfo.value(f, sr, bpm) + 1.0).abs() < 1e-9,
                "square second half should be -1 at frame {f}",
            );
        }
        // Next period boundary back to +1.
        assert!((lfo.value(period, sr, bpm) - 1.0).abs() < 1e-9);
    }

    /// Triangle peaks at 1/4 period, troughs at 3/4 period, returns to
    /// 0 at boundaries. Bounded ±1 throughout.
    #[test]
    fn triangle_peak_trough_and_bounded() {
        let sr: u32 = 48_000;
        let bpm = 120.0;
        let lfo = Lfo::new(LfoConfig::new(Shape::Triangle, RateDiv::Beat, 1.0, 0));
        let period = Lfo::period_frames(RateDiv::Beat, bpm, sr).unwrap();
        // Boundary samples → 0.
        assert!(lfo.value(0, sr, bpm).abs() < 1e-3);
        // 1/4 period → peak +1.
        assert!((lfo.value(period / 4, sr, bpm) - 1.0).abs() < 1e-3);
        // 1/2 period → 0.
        assert!(lfo.value(period / 2, sr, bpm).abs() < 1e-3);
        // 3/4 period → trough -1.
        assert!((lfo.value(3 * period / 4, sr, bpm) + 1.0).abs() < 1e-3);
        // Bounded ±1 across the whole period.
        for f in 0..period {
            let v = lfo.value(f, sr, bpm);
            assert!(
                (-1.001..=1.001).contains(&v),
                "triangle out of bounds at frame {f}: {v}",
            );
        }
    }

    /// BPM-synced 1/4 at 120 BPM = 0.125 s (= 1/4 of a beat at 120 BPM,
    /// where beat = 0.5 s). At 48 kHz that's 6000 frames per LFO cycle.
    /// Spec asks "1/4 at 120 BPM = 0.5 sec period" — the spec is using
    /// `Beat` for "1/4" (= one quarter-note = one beat in 4/4). We
    /// expose both: `Beat = 1 beat per cycle` (= 0.5 s @ 120 BPM) and
    /// `Quarter = 1/4 beat per cycle` (= 0.125 s @ 120 BPM). Verify both
    /// readings so the wire contract is unambiguous.
    #[test]
    fn bpm_sync_period_matches_spec_120bpm() {
        let sr: u32 = 48_000;
        // Spec reading: "1/4 = quarter-note = one beat" → 0.5 s period.
        let period_beat = Lfo::period_frames(RateDiv::Beat, 120.0, sr).unwrap();
        assert_eq!(period_beat, 24_000, "Beat @ 120 BPM = 0.5 s = 24000 frames");
        // Alternative reading: "1/4 = 1/4 of a beat" → 0.125 s period.
        let period_quarter = Lfo::period_frames(RateDiv::Quarter, 120.0, sr).unwrap();
        assert_eq!(
            period_quarter, 6_000,
            "Quarter (1/4 beat) @ 120 BPM = 0.125 s"
        );
        // Bar at 120 BPM = 4 beats = 2 s = 96000 frames.
        let period_bar = Lfo::period_frames(RateDiv::Bar, 120.0, sr).unwrap();
        assert_eq!(period_bar, 96_000);
        // 1/16 at 120 BPM = 1/16 beat = 0.03125 s = 1500 frames.
        let period_16 = Lfo::period_frames(RateDiv::Sixteenth, 120.0, sr).unwrap();
        assert_eq!(period_16, 1500);
    }

    /// LFO modulating filter cutoff swings around the set point. With
    /// depth=1.0 and ±2-octave range, the swept cutoff at value=+1 is
    /// 4× the base; at value=-1 it's 1/4× the base.
    #[test]
    fn lfo_modulates_filter_cutoff_in_octaves() {
        let sr: u32 = 48_000;
        let bpm = 240.0; // 1 Hz Bar cycle
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Bar, 1.0, 0));
        let mut base = [0.0_f32; super::super::MAX_PARAMS];
        base[0] = 1000.0; // cutoff_hz
        base[1] = 0.3; // resonance
        base[2] = 0.0; // LP mode
                       // At 1/4 period sine = +1.0 → cutoff multiplier 2^2 = 4×.
        let modulated = apply_lfo_to_params(&base, &lfo, EFFECT_FILTER, sr as u64 / 4, sr, bpm);
        // Filter's descriptor clamps cutoff to ≤ 20_000 Hz; 1000 × 4 = 4000 (well inside).
        assert!(
            (modulated[0] - 4000.0).abs() / 4000.0 < 0.01,
            "expected ~4000 Hz at +1 modulation, got {}",
            modulated[0]
        );
        // At 3/4 period sine = -1.0 → cutoff × 2^(-2) = 0.25× = 250 Hz.
        let modulated2 =
            apply_lfo_to_params(&base, &lfo, EFFECT_FILTER, 3 * sr as u64 / 4, sr, bpm);
        assert!(
            (modulated2[0] - 250.0).abs() / 250.0 < 0.01,
            "expected ~250 Hz at -1 modulation, got {}",
            modulated2[0]
        );
        // Untouched params unchanged.
        assert_eq!(modulated[1], 0.3);
        assert_eq!(modulated[2], 0.0);
    }

    /// LFO targeting gate `duty` (additive, scaled to descriptor range).
    /// Descriptor range for duty is `[0, 1]`; at depth=1 and sine=+1
    /// the modulation adds `1.0 * 0.5 = 0.5` to the base, clamped to
    /// the descriptor max.
    #[test]
    fn lfo_modulates_gate_duty_additive() {
        let sr: u32 = 48_000;
        let bpm = 240.0;
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Bar, 1.0, 1));
        let mut base = [0.0_f32; super::super::MAX_PARAMS];
        base[0] = 1.0; // period_div = 1/4 beat
        base[1] = 0.5; // duty 0.5
                       // At 1/4 period sine = +1.0 → duty + 0.5 = 1.0 (clamped to max).
        let modulated = apply_lfo_to_params(&base, &lfo, EFFECT_GATE, sr as u64 / 4, sr, bpm);
        assert!(
            (modulated[1] - 1.0).abs() < 1e-3,
            "expected duty 1.0 at peak (clamped), got {}",
            modulated[1]
        );
        // At 3/4 period sine = -1.0 → duty - 0.5 = 0.0 (clamped to min).
        let modulated2 = apply_lfo_to_params(&base, &lfo, EFFECT_GATE, 3 * sr as u64 / 4, sr, bpm);
        assert!(
            modulated2[1].abs() < 1e-3,
            "expected duty 0.0 at trough (clamped), got {}",
            modulated2[1]
        );
    }

    /// Depth 0 → no modulation. Required so the UI can disable an LFO
    /// without un-configuring it.
    #[test]
    fn depth_zero_is_passthrough() {
        let sr: u32 = 48_000;
        let bpm = 120.0;
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Beat, 0.0, 0));
        let mut base = [0.0_f32; super::super::MAX_PARAMS];
        base[0] = 1000.0;
        for frame in [0u64, 1000, 5000, 23999] {
            let out = apply_lfo_to_params(&base, &lfo, EFFECT_FILTER, frame, sr, bpm);
            assert_eq!(out[0], 1000.0);
        }
    }

    /// Zero / negative / NaN BPM must not panic and must produce 0
    /// (passthrough — no modulation applied).
    #[test]
    fn degenerate_bpm_yields_zero_value() {
        let sr: u32 = 48_000;
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Beat, 1.0, 0));
        for bpm in [0.0, -42.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let v = lfo.value(100, sr, bpm);
            assert_eq!(v, 0.0, "expected 0 for degenerate bpm={bpm}, got {v}");
        }
        // apply_lfo_to_params likewise returns base unchanged.
        let mut base = [0.0_f32; super::super::MAX_PARAMS];
        base[0] = 500.0;
        let out = apply_lfo_to_params(&base, &lfo, EFFECT_FILTER, 100, sr, 0.0);
        assert_eq!(out[0], 500.0);
    }

    /// `apply_lfo_to_params` is alloc-free — single block evaluation
    /// runs on the audio thread.
    #[test]
    fn assert_no_alloc_per_block_eval() {
        let sr: u32 = 48_000;
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Beat, 0.7, 0));
        let mut base = [0.0_f32; super::super::MAX_PARAMS];
        base[0] = 800.0;
        base[1] = 0.4;
        assert_no_alloc::assert_no_alloc(|| {
            // Simulate a render block calling once per buffer × 64
            // buffers (= ~700 ms @ 48 kHz, 2048 frames/block). Worst-case
            // per-block cost is dominated by the sin call.
            for buf in 0..64 {
                let frame = (buf as u64) * 2048;
                let _ = apply_lfo_to_params(&base, &lfo, EFFECT_FILTER, frame, sr, 120.0);
                let _ = lfo.value(frame, sr, 120.0);
            }
        });
    }

    /// Reverb room_size (additive, scaled by half the descriptor range).
    /// Descriptor range = 1.0 (max 1, min 0). At depth=0.5 the swing is
    /// ±0.5 × 0.5 × 1.0 = ±0.25 → modulated room_size between 0.25 and
    /// 0.75 around a base of 0.5. (At depth=1.0 the additive swing is
    /// ±0.5, which clamps at the descriptor edges — see the second
    /// branch.)
    #[test]
    fn lfo_modulates_reverb_room_size_additive() {
        let sr: u32 = 48_000;
        let bpm = 240.0;
        let lfo_half = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Bar, 0.5, 0));
        let mut base = [0.0_f32; super::super::MAX_PARAMS];
        base[0] = 0.5;
        let m_peak = apply_lfo_to_params(&base, &lfo_half, EFFECT_REVERB, sr as u64 / 4, sr, bpm);
        assert!(
            (m_peak[0] - 0.75).abs() < 1e-3,
            "expected room_size 0.75 at peak (depth=0.5), got {}",
            m_peak[0]
        );
        let m_trough =
            apply_lfo_to_params(&base, &lfo_half, EFFECT_REVERB, 3 * sr as u64 / 4, sr, bpm);
        assert!(
            (m_trough[0] - 0.25).abs() < 1e-3,
            "expected room_size 0.25 at trough (depth=0.5), got {}",
            m_trough[0]
        );
        // depth=1.0 swings the full half-range — clamped at descriptor edges.
        let lfo_full = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Bar, 1.0, 0));
        let m_full_peak =
            apply_lfo_to_params(&base, &lfo_full, EFFECT_REVERB, sr as u64 / 4, sr, bpm);
        assert!(
            (m_full_peak[0] - 1.0).abs() < 1e-3,
            "depth=1.0 peak should clamp to 1.0, got {}",
            m_full_peak[0]
        );
        let m_full_trough =
            apply_lfo_to_params(&base, &lfo_full, EFFECT_REVERB, 3 * sr as u64 / 4, sr, bpm);
        assert!(
            m_full_trough[0].abs() < 1e-3,
            "depth=1.0 trough should clamp to 0.0, got {}",
            m_full_trough[0]
        );
    }

    /// Echo time_ms — multiplicative in halvings (±1 octave at depth=1).
    /// At depth=0.5 the swing is ±0.5 octave: peak × 2^0.5 ≈ 1.414, trough × 2^-0.5 ≈ 0.707.
    #[test]
    fn lfo_modulates_echo_time_multiplicative() {
        let sr: u32 = 48_000;
        let bpm = 240.0;
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Bar, 0.5, 0));
        let mut base = [0.0_f32; super::super::MAX_PARAMS];
        base[0] = 250.0; // time_ms
        let m_peak = apply_lfo_to_params(&base, &lfo, EFFECT_ECHO, sr as u64 / 4, sr, bpm);
        let expected_peak = 250.0_f32 * 2f32.powf(0.5);
        assert!(
            (m_peak[0] - expected_peak).abs() / expected_peak < 0.01,
            "expected ~{expected_peak} ms at peak, got {}",
            m_peak[0]
        );
    }

    /// Out-of-range `target_param` is a silent passthrough — defensive
    /// behaviour mirrors `FxBank::set_param`.
    #[test]
    fn out_of_range_target_passthrough() {
        let sr: u32 = 48_000;
        let bpm = 120.0;
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Beat, 1.0, 99));
        let mut base = [0.0_f32; super::super::MAX_PARAMS];
        base[0] = 1000.0;
        let out = apply_lfo_to_params(&base, &lfo, EFFECT_FILTER, 1000, sr, bpm);
        assert_eq!(out, base, "out-of-range target_param must be no-op");
    }

    /// Wire-byte round-trip for Shape + RateDiv. Defensive: unknown
    /// bytes fall back to a sane default rather than panicking.
    #[test]
    fn shape_and_ratediv_byte_round_trip() {
        for s in [Shape::Sine, Shape::Saw, Shape::Square, Shape::Triangle] {
            assert_eq!(Shape::from_byte(s as u8), s);
            // Wire labels are kebab-case stable.
            assert!(!s.as_str().is_empty());
        }
        assert_eq!(Shape::from_byte(99), Shape::Sine);
        for r in [
            RateDiv::Bar,
            RateDiv::Beat,
            RateDiv::Half,
            RateDiv::Quarter,
            RateDiv::Eighth,
            RateDiv::Sixteenth,
        ] {
            assert_eq!(RateDiv::from_byte(r as u8), r);
            assert!(!r.as_str().is_empty());
            assert!(r.beats_per_cycle() > 0.0);
        }
        assert_eq!(RateDiv::from_byte(99), RateDiv::Quarter);
    }

    /// `lfo_value_from_clock` reads BPM + frame off the shared clock.
    /// Compile-time enforcement that the helper exists and the type
    /// signature matches; runtime sanity check on a known sine point.
    #[test]
    fn lfo_value_from_clock_reads_shared_state() {
        let clock = SharedClock::with_bpm(240.0);
        // Advance the clock to 1/4 of a 1-Hz LFO cycle (12000 frames @
        // 48 kHz Bar @ 240 BPM = 1s/cycle).
        clock.advance(12_000);
        let lfo = Lfo::new(LfoConfig::new(Shape::Sine, RateDiv::Bar, 1.0, 0));
        let v = lfo_value_from_clock(&lfo, &clock, 48_000);
        assert!((v - 1.0).abs() < 1e-3, "expected sin(π/2) = 1, got {v}");
    }
}
