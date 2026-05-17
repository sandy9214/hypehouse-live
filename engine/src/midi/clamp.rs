//! Defensive clamping for every MIDI value before it becomes an `Event`.
//!
//! Hardware controllers — especially cheap or buggy ones — can and do
//! transmit bytes outside the conventional 0..=127 range, or NaN-equivalent
//! 14-bit pitch-bend payloads. The engine MUST NEVER panic / crash / drift
//! into an inconsistent state regardless of input. All conversions from raw
//! MIDI to engine-space `f32` / `u8` values go through this module.
//!
//! Strategy:
//! * Raw 7-bit MIDI bytes are saturated to 0..=127 before any math.
//! * 14-bit pitch-bend (data1 + data2 LSB first) is reconstructed and clamped
//!   to 0..=16383 then mapped to a symmetric ±range around 0.
//! * Output `f32` values are checked for `is_finite()` and clamped to the
//!   action's documented range. NaN is impossible to produce from finite
//!   integer inputs but a malformed `range_db` (already rejected by mapping
//!   validation) is still guarded here as defense-in-depth.
//! * Hot-cue slots are clamped to 0..=7.
//!
//! This is the primary defense; `EngineState::apply` does a second clamp
//! pass for belt-and-braces. Both layers must stay in sync.

/// Standard pro EQ range used as default. Mapping-level overrides take precedence.
pub const DEFAULT_EQ_DB_LO: f32 = -26.0;
pub const DEFAULT_EQ_DB_HI: f32 = 6.0;

pub const PITCH_BEND_HARD_MAX_SEMITONES: f32 = 12.0;
pub const PITCH_BEND_HARD_MIN_SEMITONES: f32 = -12.0;

/// Saturate any unsigned byte to the legal 7-bit MIDI data range.
#[inline]
pub fn clamp_midi_byte(b: u8) -> u8 {
    b & 0x7F
}

/// Linear map 0..=127 → 0.0..=1.0 (inclusive, exact at endpoints).
#[inline]
pub fn cc_to_unit(value: u8) -> f32 {
    (clamp_midi_byte(value) as f32) / 127.0
}

/// Linear map 0..=127 → `[lo, hi]` with safe range fallback.
#[inline]
pub fn cc_to_range(value: u8, lo: f32, hi: f32) -> f32 {
    let (lo, hi) = safe_range(lo, hi, DEFAULT_EQ_DB_LO, DEFAULT_EQ_DB_HI);
    let t = cc_to_unit(value);
    let raw = lo + t * (hi - lo);
    raw.clamp(lo, hi)
}

/// 14-bit pitch-bend (LSB:MSB per MIDI spec) → semitones in `[-range, +range]`.
/// Center value is `0x2000` (8192), corresponding to 0 semitones.
#[inline]
pub fn pitch_bend_14_to_semitones(lsb: u8, msb: u8, range_semitones: f32) -> f32 {
    let lsb = clamp_midi_byte(lsb) as u16;
    let msb = clamp_midi_byte(msb) as u16;
    let raw14 = (msb << 7) | lsb; // 0..=16383
    let centered = raw14 as i32 - 8192; // -8192..=8191
    let normalized = if centered >= 0 {
        centered as f32 / 8191.0
    } else {
        centered as f32 / 8192.0
    };
    let safe_range = if range_semitones.is_finite() && range_semitones > 0.0 {
        range_semitones
    } else {
        2.0
    };
    (normalized * safe_range).clamp(PITCH_BEND_HARD_MIN_SEMITONES, PITCH_BEND_HARD_MAX_SEMITONES)
}

/// Clamp a hot-cue slot to the engine-supported 0..=7 range.
#[inline]
pub fn clamp_hot_cue_slot(slot: u8) -> u8 {
    slot.min(7)
}

fn safe_range(lo: f32, hi: f32, default_lo: f32, default_hi: f32) -> (f32, f32) {
    if lo.is_finite() && hi.is_finite() && hi > lo {
        (lo, hi)
    } else {
        (default_lo, default_hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_clamp_strips_high_bit() {
        assert_eq!(clamp_midi_byte(0x7F), 0x7F);
        assert_eq!(clamp_midi_byte(0xFF), 0x7F);
        assert_eq!(clamp_midi_byte(0x80), 0x00);
    }

    #[test]
    fn cc_to_unit_endpoints() {
        assert_eq!(cc_to_unit(0), 0.0);
        assert!((cc_to_unit(127) - 1.0).abs() < 1e-6);
        assert!((cc_to_unit(64) - (64.0 / 127.0)).abs() < 1e-6);
    }

    #[test]
    fn cc_to_range_eq() {
        // 64 ≈ middle → ~ -10 dB on a [-26, 6] scale.
        let v = cc_to_range(64, -26.0, 6.0);
        assert!(v > -11.0 && v < -9.0, "got {v}");
        assert_eq!(cc_to_range(0, -26.0, 6.0), -26.0);
        assert!((cc_to_range(127, -26.0, 6.0) - 6.0).abs() < 1e-4);
    }

    #[test]
    fn cc_to_range_inverted_falls_back_to_default() {
        // Invalid range: hi <= lo → falls back to default eq range.
        let v = cc_to_range(64, 6.0, -26.0);
        assert!(v > -11.0 && v < -9.0, "got {v}");
    }

    #[test]
    fn cc_to_range_nan_falls_back() {
        let v = cc_to_range(64, f32::NAN, 6.0);
        assert!(v.is_finite());
    }

    #[test]
    fn pitch_bend_center_is_zero() {
        let v = pitch_bend_14_to_semitones(0x00, 0x40, 2.0);
        assert!(v.abs() < 1e-6, "got {v}");
    }

    #[test]
    fn pitch_bend_max_is_positive_range() {
        // 14-bit max = 0x3FFF (lsb=0x7F, msb=0x7F) → +range
        let v = pitch_bend_14_to_semitones(0x7F, 0x7F, 2.0);
        assert!((v - 2.0).abs() < 1e-3, "got {v}");
    }

    #[test]
    fn pitch_bend_min_is_negative_range() {
        let v = pitch_bend_14_to_semitones(0x00, 0x00, 2.0);
        assert!((v + 2.0).abs() < 1e-3, "got {v}");
    }

    #[test]
    fn pitch_bend_hostile_bytes_dont_explode() {
        // High bit set on raw bytes (illegal but seen in the wild) → still clamps.
        let v = pitch_bend_14_to_semitones(0xFF, 0xFF, 2.0);
        assert!(v.is_finite());
        assert!(v.abs() <= 2.0 + 1e-3);
    }

    #[test]
    fn pitch_bend_negative_range_falls_back() {
        let v = pitch_bend_14_to_semitones(0x7F, 0x7F, -1.0);
        // Negative range_semitones falls back to default 2.0
        assert!((v - 2.0).abs() < 1e-3, "got {v}");
    }

    #[test]
    fn hot_cue_slot_clamps() {
        assert_eq!(clamp_hot_cue_slot(0), 0);
        assert_eq!(clamp_hot_cue_slot(7), 7);
        assert_eq!(clamp_hot_cue_slot(8), 7);
        assert_eq!(clamp_hot_cue_slot(255), 7);
    }
}
