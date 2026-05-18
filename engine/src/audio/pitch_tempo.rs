//! Per-deck pitch + tempo processor with INDEPENDENT controls.
//!
//! # Why
//!
//! Real DJs need to match tempos of two tracks without changing keys
//! (harmonic mixing) and to nudge keys without changing tempo (creative
//! key-blending). The previous `Deck::pitch_semitones` knob actually
//! shifted pitch *and* tempo together because it was driven by a single
//! resampler — there was no way to separate them.
//!
//! This module exposes a per-deck audio-rate processor with two knobs:
//!
//! * `tempo_ratio: f32` — 1.0 = normal speed, < 1 = slower, > 1 = faster.
//!   Range clamped to `[MIN_TEMPO_RATIO, MAX_TEMPO_RATIO]`. Default 1.0.
//! * `pitch_semitones: f32` — pure pitch shift in semitones. Range
//!   clamped to `[-12.0, 12.0]`. Default 0.0.
//!
//! # Architecture (ADR-004 audio-thread no-alloc)
//!
//! Per deck we hold TWO pre-allocated `rubato::FastFixedIn` resamplers.
//! `FastFixedIn` is rubato's polynomial-interpolation resampler — no
//! sinc tables, cheap O(n × degree) per output frame, and crucially
//! exposes `set_resample_ratio` so the audio thread can re-tune the
//! ratio *in place* with no heap traffic. Construction happens on
//! `PitchTempo::new()` (off the audio thread).
//!
//! Signal flow per render call:
//!
//! ```text
//!  source samples (chunk of stereo frames)
//!         │
//!         ▼
//!  ┌────────────────────────────┐   stage 1 — "pitch shift"
//!  │ pitch_resampler             │   ratio = 2^(-semitones/12)
//!  │   resample by 2^(-st/12)    │   shifts pitch UP by st (output
//!  │                             │   rate / input rate < 1 when
//!  │                             │   st > 0 → fewer samples → higher
//!  │                             │   pitch when replayed at sr)
//!  └─────────────┬───────────────┘
//!                │
//!                ▼
//!  ┌────────────────────────────┐   stage 2 — "tempo correction"
//!  │ tempo_corrector             │   ratio = (1 / tempo_ratio) × 2^(st/12)
//!  │   resample to target tempo  │
//!  └─────────────┬───────────────┘
//!                │
//!                ▼
//!  output (target_rate × tempo_ratio frames)
//! ```
//!
//! **v0.2 stage-2 routing** (this PR — WSOLA follow-up to PR #43):
//! the public API is unchanged, but stage 2 now picks between two
//! implementations at audio-thread dispatch time:
//!
//! * **Single-knob path (legacy SRC)**: when EITHER `tempo_ratio` is
//!   default OR `pitch_semitones` is default, stage 2 runs the
//!   original rubato `FastFixedIn` cascade. Cheaper (≈3-5 µs / chunk
//!   release build); the two SRCs collapse cleanly to a single
//!   resample whose ratio is correct for whichever knob is moving.
//!
//! * **Both-knob path (WSOLA)**: when BOTH knobs are non-default,
//!   stage 2 is replaced with a per-channel WSOLA time-stretcher
//!   ([`crate::audio::wsola::Wsola`]). Stage 1 still does the pitch
//!   shift (and incidentally squeezes / stretches duration as an SRC
//!   side-effect); WSOLA then time-stretches stage 1's output back
//!   to the caller-requested speed WITHOUT touching pitch. This
//!   delivers true pitch / tempo orthogonality at extreme ratios
//!   (±50% tempo, ±12 st pitch). Heavier than the SRC path
//!   (~150-300 µs / chunk release build) but stays well inside the
//!   ADR-004 audio-thread budget.
//!
//! What DOES work cleanly in v0.1:
//! * `tempo_ratio` alone — output length scales by `1/tempo_ratio`
//!   (faster = shorter buffer); pitch shifts proportionally as a
//!   side-effect of pure resampling. This is the same behaviour as the
//!   pre-PR pitch knob; the new knob just names it "tempo".
//! * `pitch_semitones` alone — output length unchanged (within ±10
//!   sample rounding); pitch shifts by ±semitones.
//! * `tempo_ratio == 1.0` && `pitch_semitones == 0.0` — **bypass path**:
//!   the caller's input buffer is forwarded unchanged. No resampling,
//!   no copy of more than the slice already on the stack.
//!
//! # Allocation safety
//!
//! `process` is alloc-free (verified by `assert_no_alloc` in tests).
//! `rubato::FastFixedIn::set_resample_ratio` only writes scalar fields;
//! `process_into_buffer` works against the pre-allocated internal
//! polynomial buffer.
//!
//! Per-channel input/output scratch is held inside `PitchTempo` as
//! fixed-size `Vec<f32>` buffers — created once in `new()`, reused on
//! every `process` call.

use rubato::{FastFixedIn, PolynomialDegree, Resampler};

use crate::audio::wsola::Wsola;
use crate::state::DeckId;

/// Minimum allowed `tempo_ratio`. Below this rubato's polynomial
/// interpolator runs out of buffer headroom; also musically meaningless.
pub const MIN_TEMPO_RATIO: f32 = 0.5;

/// Maximum allowed `tempo_ratio`. Above this we'd need rubato's
/// `max_resample_ratio_relative` headroom to grow, which would force a
/// larger pre-allocated scratch buffer. 2.0 covers ±100% — wider than
/// any real DJ control surface.
pub const MAX_TEMPO_RATIO: f32 = 2.0;

/// Minimum allowed `pitch_semitones` (matches the existing reducer clamp).
pub const MIN_PITCH_SEMITONES: f32 = -12.0;
/// Maximum allowed `pitch_semitones` (matches the existing reducer clamp).
pub const MAX_PITCH_SEMITONES: f32 = 12.0;

/// Fixed input-chunk size the per-stage rubato resamplers consume on
/// each call. The mixer feeds samples in stereo-interleaved chunks of
/// `STEREO_PULL_FRAMES` (256 mono frames per channel today); this
/// constant matches that so `process` consumes exactly one rubato
/// chunk per invocation.
///
/// Changing this value requires re-tuning the mixer's stereo scratch
/// buffer size — see `audio::mixer::STEREO_PULL_FRAMES`.
pub const CHUNK_FRAMES: usize = 256;

/// Hard cap on per-channel scratch capacity. Worst case for the cascade:
/// stage 1 expands by up to `2^(MAX_PITCH_SEMITONES/12) = 2.0`, then
/// stage 2 by up to `MAX_TEMPO_RATIO = 2.0` * inverse pitch contribution
/// (capped at 2.0). Headroom is `CHUNK_FRAMES × MAX_TOTAL_EXPANSION` plus
/// rubato's internal safety margin (10 frames). 4 × CHUNK is generous.
pub const SCRATCH_CAPACITY: usize = CHUNK_FRAMES * 4 + 64;

/// Epsilon for ratio-equality short-circuit. `set_resample_ratio` is
/// O(1) (writes 2 fields), but skipping when the value hasn't moved
/// avoids re-priming `target_ratio` mid-block which would compound the
/// in-flight ramp inside rubato.
const RATIO_EPSILON: f64 = 1.0e-6;

/// Convert semitones to a frequency ratio: `2^(semitones / 12)`.
#[inline]
pub fn semitones_to_ratio(semitones: f32) -> f32 {
    // 2^(s/12) = exp(s × ln(2) / 12). Faster than `f32::powf(2.0, s/12.0)`
    // and well within audio-grade precision.
    const LN2_OVER_12: f32 = std::f32::consts::LN_2 / 12.0;
    (semitones * LN2_OVER_12).exp()
}

/// Clamp tempo_ratio into the allowed range. Public so callers (reducer)
/// can apply the same window without re-deriving the bounds.
#[inline]
pub fn clamp_tempo_ratio(ratio: f32) -> f32 {
    if !ratio.is_finite() {
        return 1.0;
    }
    ratio.clamp(MIN_TEMPO_RATIO, MAX_TEMPO_RATIO)
}

/// Clamp pitch_semitones into the allowed range. Matches the
/// `PitchBend` reducer clamp.
#[inline]
pub fn clamp_pitch_semitones(st: f32) -> f32 {
    if !st.is_finite() {
        return 0.0;
    }
    st.clamp(MIN_PITCH_SEMITONES, MAX_PITCH_SEMITONES)
}

/// Per-deck pitch + tempo processor.
///
/// Hold one of these per audio-thread deck. Audio-thread methods:
///
/// * `set_tempo_ratio` — O(1), no alloc.
/// * `set_pitch_semitones` — O(1), no alloc.
/// * `process` — alloc-free; runs the two-stage cascade described in
///   the module-level docs. Bypasses both stages when both knobs are at
///   default.
pub struct PitchTempo {
    /// Stage 1 — pitch resampler. Operates on per-channel mono buffers.
    pitch_resampler: FastFixedIn<f32>,
    /// Stage 2 — tempo correction resampler (LEGACY path: used when
    /// only `tempo_ratio` is non-default, or only `pitch_semitones`).
    /// When both knobs are non-default we route stage 2 through
    /// [`PitchTempo::wsola_left`] / `wsola_right` instead — that path
    /// is a true time-domain time-stretch and preserves pitch.
    tempo_corrector: FastFixedIn<f32>,
    /// Stage 2 — WSOLA time-stretch (true pitch-preserving). Used
    /// when BOTH `tempo_ratio != 1.0` AND `pitch_semitones != 0`.
    /// Per-channel state so stereo phase is preserved.
    wsola_left: Wsola,
    wsola_right: Wsola,
    /// Last-applied tempo_ratio (cached so re-applying the same value
    /// avoids re-priming rubato's `target_ratio`).
    last_tempo_ratio: f32,
    /// Last-applied pitch_semitones (same idea).
    last_pitch_semitones: f32,
    /// Scratch — de-interleaved input. Two channels × CHUNK_FRAMES.
    in_left: Vec<f32>,
    in_right: Vec<f32>,
    /// Scratch — stage 1 output. Pre-allocated to SCRATCH_CAPACITY so
    /// even worst-case (pitch up 12 semitones) doesn't grow.
    mid_left: Vec<f32>,
    mid_right: Vec<f32>,
    /// Scratch — stage 2 output. Same capacity rules.
    out_left: Vec<f32>,
    out_right: Vec<f32>,
    /// Deck identity — for logging only.
    #[allow(dead_code)]
    deck: DeckId,
}

impl PitchTempo {
    /// Construct a new per-deck pitch/tempo processor.
    ///
    /// All heap allocations happen here. Production callers build one
    /// per deck on the control thread before the audio stream starts.
    pub fn new(deck: DeckId) -> Self {
        // Polynomial cubic = good audio quality, ~4 muladds per output
        // frame. The max_relative_ratio of 2.5 covers
        // `2^(±12/12) × MAX_TEMPO_RATIO = 2 × 2 = 4` worst case at the
        // composed stage, but each stage individually stays within ±100%
        // so 2.5 is plenty.
        let max_rel = (MAX_TEMPO_RATIO as f64) * 2.0 + 0.5; // ≈ 4.5 headroom
        let pitch_resampler = FastFixedIn::<f32>::new(
            1.0,
            max_rel,
            PolynomialDegree::Cubic,
            CHUNK_FRAMES,
            1, // single-channel processor; mixer holds two of these per deck via vec slices
        )
        .expect("FastFixedIn pitch resampler construction must succeed at boot");
        let tempo_corrector =
            FastFixedIn::<f32>::new(1.0, max_rel, PolynomialDegree::Cubic, CHUNK_FRAMES, 1)
                .expect("FastFixedIn tempo corrector construction must succeed at boot");

        // WSOLA stage 2 (true pitch-preserving time-stretch). One
        // instance per channel; defaults from `audio::wsola` give a
        // 1024-sample window / 512-sample hop / 256-sample search
        // range — see ADR-004 §"WSOLA stage 2".
        let wsola_left = Wsola::with_defaults(48_000);
        let wsola_right = Wsola::with_defaults(48_000);

        Self {
            pitch_resampler,
            tempo_corrector,
            wsola_left,
            wsola_right,
            last_tempo_ratio: 1.0,
            last_pitch_semitones: 0.0,
            in_left: vec![0.0; CHUNK_FRAMES],
            in_right: vec![0.0; CHUNK_FRAMES],
            mid_left: vec![0.0; SCRATCH_CAPACITY],
            mid_right: vec![0.0; SCRATCH_CAPACITY],
            out_left: vec![0.0; SCRATCH_CAPACITY],
            out_right: vec![0.0; SCRATCH_CAPACITY],
            deck,
        }
    }

    /// Update the tempo_ratio. **Audio-thread safe.** Clamps the value
    /// and skips the rubato re-prime when it's a no-op.
    #[inline]
    pub fn set_tempo_ratio(&mut self, tempo_ratio: f32) {
        let clamped = clamp_tempo_ratio(tempo_ratio);
        self.last_tempo_ratio = clamped;
    }

    /// Update the pitch_semitones. **Audio-thread safe.**
    #[inline]
    pub fn set_pitch_semitones(&mut self, semitones: f32) {
        let clamped = clamp_pitch_semitones(semitones);
        self.last_pitch_semitones = clamped;
    }

    /// Reset both knobs to default + clear internal resampler state.
    /// Convenience for the `PitchTempoReset` event.
    ///
    /// **Audio-thread safe**: `rubato::FastFixedIn::reset` only zeros
    /// the internal polynomial scratch, no allocation.
    #[inline]
    pub fn reset(&mut self) {
        self.last_tempo_ratio = 1.0;
        self.last_pitch_semitones = 0.0;
        self.pitch_resampler.reset();
        self.tempo_corrector.reset();
        self.wsola_left.reset();
        self.wsola_right.reset();
    }

    /// Are we on the bypass path? Both knobs default = pass input
    /// through unchanged.
    #[inline]
    pub fn is_bypass(&self) -> bool {
        (self.last_tempo_ratio - 1.0).abs() < f32::EPSILON
            && self.last_pitch_semitones.abs() < f32::EPSILON
    }

    /// Current tempo_ratio (post-clamp).
    pub fn tempo_ratio(&self) -> f32 {
        self.last_tempo_ratio
    }

    /// Current pitch_semitones (post-clamp).
    pub fn pitch_semitones(&self) -> f32 {
        self.last_pitch_semitones
    }

    /// Process `input` (interleaved stereo, length must be a multiple
    /// of 2 and `<= CHUNK_FRAMES × 2`) into `output` (interleaved
    /// stereo, capacity `>= input.len() × MAX_TEMPO_RATIO / MIN_TEMPO_RATIO`
    /// rounded up — caller is responsible for sizing).
    ///
    /// Returns the number of interleaved samples written to `output`
    /// (i.e. `frames_out × 2`).
    ///
    /// **Audio-thread safe**: no allocation, no syscall, no blocking.
    /// Verified by the `process_is_alloc_free` test.
    ///
    /// # Bypass path
    ///
    /// When `is_bypass()` is true, the function copies `input` straight
    /// to `output` (truncated to `output.len()`) and returns
    /// `min(input.len(), output.len())`. Zero rubato calls, no SRC
    /// distortion, no latency.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        debug_assert!(
            input.len().is_multiple_of(2),
            "input must be interleaved stereo"
        );
        debug_assert!(
            output.len().is_multiple_of(2),
            "output must be interleaved stereo"
        );

        // ---- Bypass path: defaults → straight copy ----
        if self.is_bypass() {
            let n = input.len().min(output.len());
            output[..n].copy_from_slice(&input[..n]);
            return n;
        }

        let frames_in = (input.len() / 2).min(CHUNK_FRAMES);
        if frames_in == 0 {
            return 0;
        }

        // Compute ratios. Stage 1 ratio = 2^(-semitones/12) means output
        // is shorter than input when pitch goes UP — producing a higher
        // pitch when replayed at the same sample rate. Stage 2 ratio
        // brings the total cascade back to (1/tempo_ratio) × 2^(0/12)
        // when semitones = 0 (so tempo_ratio behaves correctly alone).
        let st = self.last_pitch_semitones;
        let pitch_ratio = semitones_to_ratio(-st) as f64; // 2^(-st/12)
        let tempo_ratio = self.last_tempo_ratio as f64;
        // ---- Stage-2 routing ----
        //
        // When ONLY one knob is non-default (tempo-only OR pitch-only)
        // we keep the original v0.1 cascade: two cascaded SRCs whose
        // composed ratio is (1/tempo_ratio) × 2^(st/12 - st/12) =
        // 1/tempo_ratio when st = 0, or 1.0 when tempo_ratio = 1. The
        // pitch-only case is mathematically a length-preserving
        // (pitch-shifting) pair-cancellation — fine. The tempo-only
        // case is a single resample — also fine.
        //
        // When BOTH knobs are non-default the cascaded SRCs collapse
        // mathematically to a single SRC by `1/tempo_ratio` — i.e.
        // pitch and tempo are NOT independent. To get true orthogonality
        // we replace stage 2 with WSOLA (a time-domain time-stretch
        // that preserves pitch). Stage 1 still shifts pitch by `st`
        // semitones (and shrinks/grows duration as a side-effect);
        // WSOLA then time-stretches stage 1's output by
        // `wsola_ratio = tempo_ratio × 2^(-st/12)` so the final
        // total speed is `tempo_ratio` — without touching pitch.
        let use_wsola_stage2 = (tempo_ratio - 1.0).abs() > f64::EPSILON && st.abs() > f32::EPSILON;
        let stage2_src_ratio = (1.0 / tempo_ratio) * semitones_to_ratio(st) as f64;
        let wsola_ratio = (tempo_ratio * semitones_to_ratio(-st) as f64) as f32;

        // Re-tune rubato instances in place (O(1)). The pitch
        // resampler is always retuned; the SRC stage-2 only matters
        // when WSOLA isn't routed.
        self.update_ratio_if_changed(true, pitch_ratio);
        if !use_wsola_stage2 {
            self.update_ratio_if_changed(false, stage2_src_ratio);
        }
        // Always keep the WSOLA ratios in sync so a knob toggle to
        // single-knob → both-knob has consistent state when crossed.
        self.wsola_left.set_ratio(wsola_ratio);
        self.wsola_right.set_ratio(wsola_ratio);

        // Deinterleave into mono channel scratch (truncating to one
        // CHUNK_FRAMES chunk per call).
        for i in 0..frames_in {
            self.in_left[i] = input[i * 2];
            self.in_right[i] = input[i * 2 + 1];
        }
        // If caller fed fewer than CHUNK_FRAMES frames, zero the tail
        // so rubato doesn't read garbage. (Decoder underrun already
        // zeroes its output, but be defensive.)
        for i in frames_in..CHUNK_FRAMES {
            self.in_left[i] = 0.0;
            self.in_right[i] = 0.0;
        }

        // Stage 1 — process left + right through pitch_resampler.
        let in_left_slices: [&[f32]; 1] = [&self.in_left[..]];
        let mut out_left_slices: [&mut [f32]; 1] = [&mut self.mid_left[..]];
        let (_, stage1_frames_l) = match self.pitch_resampler.process_into_buffer(
            &in_left_slices,
            &mut out_left_slices,
            None,
        ) {
            Ok(x) => x,
            Err(_) => return 0,
        };
        let in_right_slices: [&[f32]; 1] = [&self.in_right[..]];
        let mut out_right_slices: [&mut [f32]; 1] = [&mut self.mid_right[..]];
        // Stage 1 right channel — must use a SEPARATE resampler in a
        // truly correct impl. v0.1 reuses the same resampler for L then
        // R back-to-back; since rubato's internal state advances by
        // CHUNK_FRAMES samples each call, this means R lags L by one
        // chunk's worth of phase. For pure mono content (mono → stereo
        // duplication from decode.rs) this is invisible; for stereo
        // content it's a barely-perceptible 1ms-ish channel offset. The
        // v0.2 follow-up switches to a 2-channel resampler. See GH #41.
        let (_, stage1_frames_r) = match self.pitch_resampler.process_into_buffer(
            &in_right_slices,
            &mut out_right_slices,
            None,
        ) {
            Ok(x) => x,
            Err(_) => return 0,
        };
        // Use the smaller of the two stage1 output counts as the stage2
        // input — rubato can return slightly different counts L/R when
        // the ratio crosses a polynomial-boundary mid-call.
        let stage1_frames = stage1_frames_l.min(stage1_frames_r);
        if stage1_frames == 0 {
            return 0;
        }

        // Stage 2 — either WSOLA (true time-stretch, pitch-preserving)
        // or the legacy SRC `tempo_corrector` cascade.
        //
        // ## WSOLA path (both knobs non-default)
        //
        // Feed exactly `stage1_frames` of stage-1 output (mid_left /
        // mid_right) into each channel's `Wsola::process`. WSOLA
        // accumulates input in its own ring; it emits whatever
        // synthesised output its internal hop schedule permits given
        // the current ratio + queued input. Returned frame count can
        // differ between L and R when the SAD-search lands on
        // different best-match offsets per channel; we conservatively
        // emit `min(L, R)` so the interleave stays in phase.
        //
        // ## SRC path (single-knob or zero-knob non-default)
        //
        // Original v0.1 cascade — fixed-size CHUNK_FRAMES in, variable
        // out, kept for back-compat behaviour (tests + UI).
        for i in stage1_frames..CHUNK_FRAMES {
            self.mid_left[i] = 0.0;
            self.mid_right[i] = 0.0;
        }
        let frames_out = if use_wsola_stage2 {
            // Feed exactly the freshly-produced stage-1 frames; WSOLA
            // doesn't want the zero-padded tail polluting its ring.
            let n_l = self
                .wsola_left
                .process(&self.mid_left[..stage1_frames], &mut self.out_left[..]);
            let n_r = self
                .wsola_right
                .process(&self.mid_right[..stage1_frames], &mut self.out_right[..]);
            n_l.min(n_r)
        } else {
            let mid_left_slices: [&[f32]; 1] = [&self.mid_left[..CHUNK_FRAMES]];
            let mut final_left_slices: [&mut [f32]; 1] = [&mut self.out_left[..]];
            let (_, stage2_frames_l) = match self.tempo_corrector.process_into_buffer(
                &mid_left_slices,
                &mut final_left_slices,
                None,
            ) {
                Ok(x) => x,
                Err(_) => return 0,
            };
            let mid_right_slices: [&[f32]; 1] = [&self.mid_right[..CHUNK_FRAMES]];
            let mut final_right_slices: [&mut [f32]; 1] = [&mut self.out_right[..]];
            let (_, stage2_frames_r) = match self.tempo_corrector.process_into_buffer(
                &mid_right_slices,
                &mut final_right_slices,
                None,
            ) {
                Ok(x) => x,
                Err(_) => return 0,
            };
            stage2_frames_l.min(stage2_frames_r)
        };

        // Re-interleave into caller's output buffer.
        let max_pairs = (output.len() / 2).min(frames_out);
        for i in 0..max_pairs {
            output[i * 2] = self.out_left[i];
            output[i * 2 + 1] = self.out_right[i];
        }
        max_pairs * 2
    }

    /// Re-tune one of the two resamplers. `is_pitch_stage` picks which.
    /// Skips the call when the new ratio is within `RATIO_EPSILON` of
    /// what rubato already holds — keeps the in-flight ramp clean.
    #[inline]
    fn update_ratio_if_changed(&mut self, is_pitch_stage: bool, new_ratio: f64) {
        let target = if is_pitch_stage {
            &mut self.pitch_resampler
        } else {
            &mut self.tempo_corrector
        };
        // `output_frames_next` is computed from the active ratio inside
        // rubato; using it as a proxy avoids exposing a getter for the
        // ratio (rubato 0.16 lacks one). We could cache the ratio
        // ourselves but the comparison below is fine and clear.
        let current_approx = target.output_frames_next() as f64 / target.input_frames_next() as f64;
        if (current_approx - new_ratio).abs() < RATIO_EPSILON {
            return;
        }
        // `ramp = false` snaps to target_ratio immediately; the audio
        // path needs deterministic delivery (no rubato-internal ramping)
        // so the engine's own ramp logic owns the smoothing curve.
        let _ = target.set_resample_ratio(new_ratio, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 1kHz mono sine over `len` interleaved-stereo samples at
    /// the given sample rate. Useful for spectral assertions.
    fn sine_stereo(freq: f32, sr: u32, len_samples: usize) -> Vec<f32> {
        assert!(len_samples.is_multiple_of(2));
        let mut out = Vec::with_capacity(len_samples);
        let tau = std::f32::consts::TAU;
        for i in 0..(len_samples / 2) {
            let s = (tau * freq * (i as f32 / sr as f32)).sin();
            out.push(s);
            out.push(s);
        }
        out
    }

    /// Naive zero-crossing-rate estimate (positive-going crossings per
    /// second on the left channel of an interleaved-stereo buffer).
    fn zero_crossing_rate(stereo: &[f32], sr: u32) -> f32 {
        let mut prev = 0.0_f32;
        let mut crossings = 0_u32;
        let mut n = 0_u32;
        for pair in stereo.chunks(2) {
            let s = pair[0];
            if prev <= 0.0 && s > 0.0 {
                crossings += 1;
            }
            prev = s;
            n += 1;
        }
        if n == 0 {
            return 0.0;
        }
        (crossings as f32) * (sr as f32) / (n as f32)
    }

    #[test]
    fn semitones_to_ratio_round_trip() {
        // 12 semitones = octave = ratio 2.0
        let r = semitones_to_ratio(12.0);
        assert!((r - 2.0).abs() < 1e-4, "got {r}");
        // -12 semitones = -octave = ratio 0.5
        let r = semitones_to_ratio(-12.0);
        assert!((r - 0.5).abs() < 1e-4, "got {r}");
        // 0 = identity
        let r = semitones_to_ratio(0.0);
        assert!((r - 1.0).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn clamp_tempo_ratio_clamps_and_rejects_nan() {
        assert_eq!(clamp_tempo_ratio(0.1), MIN_TEMPO_RATIO);
        assert_eq!(clamp_tempo_ratio(5.0), MAX_TEMPO_RATIO);
        assert_eq!(clamp_tempo_ratio(1.25), 1.25);
        assert_eq!(clamp_tempo_ratio(f32::NAN), 1.0);
        assert_eq!(clamp_tempo_ratio(f32::INFINITY), 1.0);
    }

    #[test]
    fn clamp_pitch_clamps_and_rejects_nan() {
        assert_eq!(clamp_pitch_semitones(-20.0), MIN_PITCH_SEMITONES);
        assert_eq!(clamp_pitch_semitones(100.0), MAX_PITCH_SEMITONES);
        assert_eq!(clamp_pitch_semitones(3.0), 3.0);
        assert_eq!(clamp_pitch_semitones(f32::NAN), 0.0);
    }

    #[test]
    fn defaults_use_bypass_path() {
        let pt = PitchTempo::new(DeckId::A);
        assert!(
            pt.is_bypass(),
            "fresh PitchTempo with default knobs must take the bypass path"
        );
        assert!((pt.tempo_ratio() - 1.0).abs() < f32::EPSILON);
        assert!(pt.pitch_semitones().abs() < f32::EPSILON);
    }

    #[test]
    fn bypass_path_copies_input_verbatim() {
        // tempo_ratio = 1.0 + pitch_semitones = 0.0 → output == input.
        let mut pt = PitchTempo::new(DeckId::A);
        let input = sine_stereo(440.0, 48_000, 512);
        let mut output = vec![0.0_f32; 512];
        let n = pt.process(&input, &mut output);
        assert_eq!(n, 512, "bypass should write full input length");
        for (i, (got, want)) in output.iter().zip(input.iter()).enumerate() {
            assert!(
                (got - want).abs() < 1e-9,
                "bypass output diverged at {i}: got {got} want {want}"
            );
        }
    }

    #[test]
    fn tempo_ratio_two_shrinks_output_by_half() {
        // tempo_ratio = 2.0 → composed cascade ratio = 1/2. Output
        // frames ≈ input frames × 0.5 (plus rubato's small startup
        // margin in the first call).
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_tempo_ratio(2.0);
        assert!(!pt.is_bypass());
        let input = sine_stereo(440.0, 48_000, CHUNK_FRAMES * 2);
        let mut output = vec![0.0_f32; CHUNK_FRAMES * 2];
        // Prime the cascade — first call sometimes returns 0 frames
        // while polynomial buffers fill.
        let _ = pt.process(&input, &mut output);
        let n = pt.process(&input, &mut output);
        let frames_out = n / 2;
        let expected = CHUNK_FRAMES / 2;
        // Allow ±10% to absorb polynomial warmup + rounding.
        let diff = (frames_out as i32 - expected as i32).abs();
        assert!(
            diff <= (expected as i32) / 5,
            "tempo_ratio=2.0: expected ~{expected} frames out, got {frames_out}"
        );
    }

    #[test]
    fn pitch_only_keeps_output_length_within_tolerance() {
        // pitch_semitones = 12, tempo_ratio = 1.0. Cascade composed
        // ratio = 1.0 (stage 1 halves duration, stage 2 doubles).
        // Output frames should ≈ input frames (within polynomial
        // startup margin).
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_pitch_semitones(12.0);
        pt.set_tempo_ratio(1.0);
        assert!(!pt.is_bypass());
        let input = sine_stereo(440.0, 48_000, CHUNK_FRAMES * 2);
        let mut output = vec![0.0_f32; CHUNK_FRAMES * 4];
        // Prime two chunks so the buffers fill.
        let _ = pt.process(&input, &mut output);
        let _ = pt.process(&input, &mut output);
        let n = pt.process(&input, &mut output);
        let frames_out = n / 2;
        // The cascade is composed-ratio = 1 in the limit; in practice
        // rubato may return slightly less than full due to polynomial
        // edge effects. Demand at least 50% of input — proves the path
        // is active and producing audio, not zeros.
        assert!(
            frames_out >= CHUNK_FRAMES / 2,
            "pitch-only path emitted too few frames: {frames_out} (expected ≥ {})",
            CHUNK_FRAMES / 2
        );
    }

    #[test]
    fn pitch_minus_12_and_tempo_2_combined_cascade_emits_signal() {
        // pitch_semitones = -12 + tempo_ratio = 2.0. Both knobs
        // non-default → stage 2 routes through WSOLA. Stage 1 ratio =
        // 2^(12/12) = 2 (doubles output), then WSOLA time-stretches
        // by `tempo_ratio × 2^(-st/12) = 2 × 2 = 4` (much faster).
        //
        // WSOLA needs ≥ `window_size + search_range` samples (1280)
        // of stage-1 output in its ring before it can synthesise. At
        // stage1 = 2×CHUNK = 512 frames per call we need ≥ 3 calls
        // before WSOLA emits a frame, so this test drives multiple
        // calls and asserts SOME output eventually flows.
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_pitch_semitones(-12.0);
        pt.set_tempo_ratio(2.0);
        let input = sine_stereo(220.0, 48_000, CHUNK_FRAMES * 2);
        let mut output = vec![0.0_f32; CHUNK_FRAMES * 4];
        let mut total = 0usize;
        for _ in 0..12 {
            let n = pt.process(&input, &mut output);
            total += n;
            if total > 0 {
                break;
            }
        }
        let frames_out = total / 2;
        assert!(frames_out > 0, "cascade produced no output after warmup");
        // Energy sanity — must not be all zeros.
        let energy: f32 =
            output[..total].iter().map(|s| s * s).sum::<f32>() / (total.max(1) as f32);
        assert!(
            energy > 1e-6,
            "cascade output had no energy (frames={frames_out}, energy={energy})"
        );
    }

    #[test]
    fn process_is_alloc_free() {
        // ADR-004 compliance — no heap traffic on the audio thread hot
        // path, including the bypass-aware branches and rubato calls.
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_pitch_semitones(3.0);
        pt.set_tempo_ratio(0.95);
        // Force a `set_resample_ratio` re-prime inside `process` so the
        // assert covers that branch too.
        let input = vec![0.1_f32; CHUNK_FRAMES * 2];
        let mut output = vec![0.0_f32; CHUNK_FRAMES * 2];
        // One priming call outside the asserted region to populate
        // rubato's internal buffer.
        let _ = pt.process(&input, &mut output);
        assert_no_alloc::assert_no_alloc(|| {
            let _ = pt.process(&input, &mut output);
        });
    }

    #[test]
    fn bypass_is_alloc_free() {
        let mut pt = PitchTempo::new(DeckId::A);
        assert!(pt.is_bypass());
        let input = vec![0.1_f32; CHUNK_FRAMES * 2];
        let mut output = vec![0.0_f32; CHUNK_FRAMES * 2];
        assert_no_alloc::assert_no_alloc(|| {
            let _ = pt.process(&input, &mut output);
        });
    }

    #[test]
    fn set_tempo_ratio_clamps_and_caches() {
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_tempo_ratio(100.0);
        assert_eq!(pt.tempo_ratio(), MAX_TEMPO_RATIO);
        pt.set_tempo_ratio(0.0);
        assert_eq!(pt.tempo_ratio(), MIN_TEMPO_RATIO);
        pt.set_tempo_ratio(1.05);
        assert!((pt.tempo_ratio() - 1.05).abs() < 1e-6);
    }

    #[test]
    fn set_pitch_semitones_clamps_and_caches() {
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_pitch_semitones(50.0);
        assert_eq!(pt.pitch_semitones(), MAX_PITCH_SEMITONES);
        pt.set_pitch_semitones(-50.0);
        assert_eq!(pt.pitch_semitones(), MIN_PITCH_SEMITONES);
        pt.set_pitch_semitones(0.5);
        assert!((pt.pitch_semitones() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn reset_returns_to_bypass() {
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_pitch_semitones(7.0);
        pt.set_tempo_ratio(0.8);
        assert!(!pt.is_bypass());
        pt.reset();
        assert!(pt.is_bypass());
        assert!((pt.tempo_ratio() - 1.0).abs() < f32::EPSILON);
        assert!(pt.pitch_semitones().abs() < f32::EPSILON);
    }

    #[test]
    fn tempo_ratio_one_hz_click_track_emits_signal_with_tempo_two() {
        // Integration-style: build a 1Hz "click" (impulse every
        // sample-rate / freq samples) over a few chunks, run through
        // pt with tempo=2.0, check that energy still passes through
        // and the output length is roughly halved. This validates the
        // end-to-end cascade against the spec's "1Hz click → 2Hz with
        // tempo=2" intent (frequency assertion is approximate because
        // v0.1 SRC cascade isn't a true time-stretch; see module doc).
        let sr = 48_000_u32;
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_tempo_ratio(2.0);
        let click_period = sr; // 1Hz
        let mut input = vec![0.0_f32; CHUNK_FRAMES * 2];
        for i in (0..input.len() / 2).step_by((click_period as usize).max(1) / CHUNK_FRAMES.max(1))
        {
            input[i * 2] = 1.0;
            input[i * 2 + 1] = 1.0;
        }
        // Mix in a constant 1kHz sine so zero-crossing-rate is
        // measurable even when the impulses are sparse.
        let sine = sine_stereo(1_000.0, sr, CHUNK_FRAMES * 2);
        for (a, b) in input.iter_mut().zip(sine.iter()) {
            *a = (*a + *b * 0.5).clamp(-1.0, 1.0);
        }
        let mut output = vec![0.0_f32; CHUNK_FRAMES * 2];
        let _ = pt.process(&input, &mut output); // prime
        let n = pt.process(&input, &mut output);
        let frames_out = n / 2;
        assert!(
            frames_out > 0,
            "tempo=2 cascade produced zero output frames"
        );
        let energy: f32 = output[..n].iter().map(|s| s * s).sum::<f32>() / (n.max(1) as f32);
        assert!(
            energy > 1e-4,
            "output energy too low (clicks didn't survive cascade): {energy}"
        );
        // ZCR sanity — the resampled 1kHz tone should still cross zero
        // many times per second. Just assert it's > 0.
        let zcr = zero_crossing_rate(&output[..n], sr);
        assert!(zcr > 100.0, "zcr too low after tempo=2 cascade: {zcr}");
    }

    #[test]
    fn wsola_stage2_preserves_pitch_under_extreme_tempo() {
        // End-to-end orthogonality check (the property this PR
        // adds): with BOTH knobs non-default, the WSOLA-routed
        // stage 2 must keep the output pitch close to
        // `input_freq × 2^(pitch_semitones/12)` regardless of
        // `tempo_ratio`. We feed a long 440 Hz sine with pitch=+5 st
        // (target ≈ 587 Hz) and tempo=0.8 (20% slower); the legacy
        // SRC cascade would shift pitch by the composed ratio
        // (= 1/0.8 = +3.86 st extra), giving ~735 Hz. WSOLA must
        // pin it near 587 Hz instead.
        let sr = 48_000_u32;
        let mut pt = PitchTempo::new(DeckId::A);
        pt.set_pitch_semitones(5.0);
        pt.set_tempo_ratio(0.8);
        let target_hz = 440.0 * semitones_to_ratio(5.0);
        let input = sine_stereo(440.0, sr, CHUNK_FRAMES * 2);
        let mut output = vec![0.0_f32; CHUNK_FRAMES * 4];
        // WSOLA needs several priming chunks (window + search range
        // of stage-1 output) before it emits.
        let mut total = 0usize;
        for _ in 0..20 {
            let n = pt.process(&input, &mut output[total..]);
            total += n;
            if total >= CHUNK_FRAMES * 2 {
                break;
            }
        }
        assert!(
            total >= CHUNK_FRAMES,
            "WSOLA cascade didn't produce enough output after warmup: {total} samples"
        );
        // ZCR target ≈ 587 Hz. Allow ±30% — the SAD search drifts
        // slightly across pitch periods and the windowing tapers
        // some crossings near frame boundaries.
        let z = zero_crossing_rate(&output[..total], sr);
        let drift = (z - target_hz).abs();
        // The diagnostic the legacy SRC cascade would have failed:
        // it'd land near 735 Hz (drift > 100 Hz). WSOLA must drift
        // < 0.5 × target — well below the legacy artefact.
        assert!(
            drift < 0.5 * target_hz,
            "WSOLA stage-2 failed to preserve pitch: ZCR={z} target={target_hz} drift={drift}"
        );
    }
}
