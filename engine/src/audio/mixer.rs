//! Audio-thread internal mixing state + sample rendering.
//!
//! Per ADR-004 §"v0.1 trivial mix": two decks, each a phase oscillator
//! (sine 440Hz / 220Hz) gated by `playing`, plus a crossfader.
//!
//! HARD RULES (ADR-004 §"Hard rules on the audio thread"):
//! * NO allocation — all state is `Copy`-shaped + held in a single
//!   `AudioMixer` struct constructed before the cpal stream starts.
//! * NO Mutex — only the lock-free SPSC ring + atomic clock.
//! * NO blocking primitives.
//!
//! Real per-track sample playback lands in a later PR; this PR proves
//! the wire is end-to-end functional.

use std::sync::Arc;

use crate::audio::clock::SharedClock;
use crate::audio::command::{AudioCommand, AudioCommandKind};
use crate::audio::decode::{DecodeHandle, DecodeService};
use crate::audio::effects::{FxBank, EFFECT_NONE};
use crate::audio::limiter::MasterLimiter;
use crate::audio::pitch_tempo::{PitchTempo, CHUNK_FRAMES as PT_CHUNK_FRAMES};
use crate::recording::MasterRecorderSink;
use crate::state::DeckId;

/// Per-callback scratch stride used when pulling stereo samples from
/// the decode service. 256 stereo frames = 512 interleaved f32 = ~5 ms
/// @ 48 kHz. Fits in `MAX_STEREO_SCRATCH` below.
const STEREO_PULL_FRAMES: usize = 256;

/// Hard cap on the per-render stereo scratch buffer. Single render
/// call cannot pull more than this from the decoder in one go;
/// `AudioMixer::render` slices the output into chunks of at most
/// `MAX_STEREO_SCRATCH / 2` mono frames.
const MAX_STEREO_SCRATCH: usize = 8192;

/// Per-deck audio-thread hot state.
#[derive(Clone, Copy, Debug)]
struct DeckHot {
    playing: bool,
    /// Phase accumulator for the v0.1 sine oscillator (radians).
    phase: f32,
    /// Tone (Hz) — 440 for A, 220 for B in v0.1.
    freq_hz: f32,
    /// Per-deck linear gain after EQ + handoff. 1.0 = full.
    gain: f32,
    /// 1-bar handoff envelope end (ADR-005); 0 = no handoff active.
    handoff_until_frame: u64,
    /// Decode handle bound to this deck (`DecodeHandle::NONE` if no
    /// track loaded). When `Some + playing`, the mixer pulls stereo
    /// frames from the decode service instead of running the
    /// fallback oscillator.
    loaded: DecodeHandle,
}

impl DeckHot {
    const fn new(freq_hz: f32) -> Self {
        Self {
            playing: false,
            phase: 0.0,
            freq_hz,
            gain: 1.0,
            handoff_until_frame: 0,
            loaded: DecodeHandle::NONE,
        }
    }
}

/// Audio-thread mixing state. Lives behind the cpal callback. Never
/// allocates after construction.
///
/// Holds an `Arc<dyn DecodeService>` so the cpal callback can pull
/// streaming samples for any currently-loaded deck. The decode
/// service's `read` is contractually alloc-free + lock-free
/// (`SymphoniaDecodeService` uses an `ArrayQueue` SPSC under the
/// hood); see `decode.rs` module docs.
pub struct AudioMixer {
    sample_rate: u32,
    crossfader: f32,
    deck_a: DeckHot,
    deck_b: DeckHot,
    /// Streaming decode service. None → fallback oscillator path
    /// (used by tests that don't wire a real service).
    decode: Option<Arc<dyn DecodeService>>,
    /// Per-render scratch buffer for stereo pulls. Allocated once at
    /// construction; the render loop only writes into a prefix of it.
    stereo_scratch: [f32; MAX_STEREO_SCRATCH],
    /// ADR-006 — per-deck 3-slot effects chain. Pre-allocated:
    /// every slot owns all 4 effect instances and dispatches by
    /// `effect_id`. Audio-thread alloc-free; switching effects just
    /// `reset()`s the target instance.
    effects_a: [FxBank; 3],
    effects_b: [FxBank; 3],
    /// Per-deck pitch + tempo processor (independent controls). Owns
    /// the rubato cascade + per-channel scratch. Pre-allocated; the
    /// audio thread only calls `set_*` / `process` / `reset`, all
    /// alloc-free.
    pitch_tempo_a: PitchTempo,
    pitch_tempo_b: PitchTempo,
    /// Output scratch for the pitch/tempo cascade. Re-used across
    /// render calls so the audio thread never allocates. Sized to the
    /// worst-case stage-2 expansion (input × MAX_TEMPO_RATIO/MIN ≈ 4×).
    pt_scratch_a: [f32; PT_OUT_SCRATCH],
    pt_scratch_b: [f32; PT_OUT_SCRATCH],
    /// Master-mix recorder sink. `None` when the user has disabled
    /// recording via `HYPEHOUSE_RECORDING_DISABLED=1` (or when tests
    /// don't wire one). The tee path inside [`AudioMixer::render`] is
    /// alloc-free: it materialises a `[L, R]` block into `rec_scratch`
    /// and pushes the slice into the recorder's lock-free ring.
    recorder: Option<MasterRecorderSink>,
    /// Per-chunk interleaved-stereo scratch fed to the recorder.
    /// Re-used across render calls. Sized to one stereo pull chunk so
    /// the tee never spills.
    rec_scratch: [f32; STEREO_PULL_FRAMES * 2],
    /// Master-bus soft-clip limiter (ADR-004 §"master-bus protection").
    /// Sits between the per-deck crossfade and the recorder tee + cpal
    /// output, so both the live mix and the recorded `master.wav` are
    /// protected against clipping when both decks are loud + effects
    /// are active. See [`crate::audio::limiter`] for the algorithm.
    limiter: MasterLimiter,
}

/// Per-deck pitch/tempo output scratch capacity (interleaved stereo
/// samples). Stage-2 expansion is capped at 4× input chunk so a 256-
/// frame input can yield up to ~1024 frames out = 2048 interleaved
/// samples. Round up for rubato's polynomial safety margin.
const PT_OUT_SCRATCH: usize = PT_CHUNK_FRAMES * 8 + 128;

impl AudioMixer {
    pub fn new(sample_rate: u32) -> Self {
        Self::with_clock(sample_rate, SharedClock::new(), 120.0)
    }

    /// Construct with a shared clock + master BPM (needed by the
    /// beat-synced Gate effect). Production: pass the same
    /// `SharedClock` the cpal callback bumps + the session BPM.
    pub fn with_clock(sample_rate: u32, clock: SharedClock, master_bpm: f32) -> Self {
        let mk_bank = || FxBank::new(sample_rate, clock.clone(), master_bpm);
        Self {
            sample_rate,
            crossfader: 0.5,
            deck_a: DeckHot::new(440.0),
            deck_b: DeckHot::new(220.0),
            decode: None,
            stereo_scratch: [0.0; MAX_STEREO_SCRATCH],
            effects_a: [mk_bank(), mk_bank(), mk_bank()],
            effects_b: [mk_bank(), mk_bank(), mk_bank()],
            pitch_tempo_a: PitchTempo::new(DeckId::A),
            pitch_tempo_b: PitchTempo::new(DeckId::B),
            pt_scratch_a: [0.0; PT_OUT_SCRATCH],
            pt_scratch_b: [0.0; PT_OUT_SCRATCH],
            recorder: None,
            rec_scratch: [0.0; STEREO_PULL_FRAMES * 2],
            limiter: MasterLimiter::new(sample_rate),
        }
    }

    /// Attach a recording sink so [`render`] tees the final master
    /// mix (as interleaved-stereo L=R duplicates of the current mono
    /// output) into the recorder. Idempotent: a second call replaces
    /// the previous sink.
    pub fn attach_recorder(&mut self, sink: MasterRecorderSink) {
        self.recorder = Some(sink);
    }

    /// Construct a mixer wired to a real decode service. Production
    /// path (`main.rs`); tests use `AudioMixer::new` to keep behaviour
    /// identical to v0.1 (oscillator-only).
    pub fn with_decode(sample_rate: u32, decode: Arc<dyn DecodeService>) -> Self {
        let mut m = Self::new(sample_rate);
        m.decode = Some(decode);
        m
    }

    /// Apply a single audio command. **Audio-thread side.** Must NOT
    /// allocate, lock, or block. The ramp_frames hint is honored
    /// trivially in v0.1 (snap-to-target); real one-pole smoothing
    /// lands in a follow-up PR.
    #[inline]
    pub fn apply(&mut self, cmd: AudioCommand) {
        match cmd.kind {
            AudioCommandKind::DeckPlay { deck } => self.deck_mut(deck).playing = true,
            AudioCommandKind::DeckPause { deck } => self.deck_mut(deck).playing = false,
            AudioCommandKind::DeckSeek { deck, .. } => {
                // No buffer-based playback yet; reset phase so the user
                // hears the cue impulse.
                self.deck_mut(deck).phase = 0.0;
            }
            AudioCommandKind::Crossfader { target, .. } => {
                self.crossfader = target.clamp(0.0, 1.0);
            }
            AudioCommandKind::EqLow {
                deck, target_db, ..
            }
            | AudioCommandKind::EqMid {
                deck, target_db, ..
            }
            | AudioCommandKind::EqHigh {
                deck, target_db, ..
            } => {
                // v0.1: EQ collapses to a single gain factor on the
                // deck since we have no real filter chain yet. dB →
                // linear, clamped.
                let lin = db_to_linear(target_db);
                self.deck_mut(deck).gain = lin;
            }
            AudioCommandKind::Pitch {
                deck, semitones, ..
            } => {
                // Pitch is now PURE pitch (independent of tempo). The
                // mixer's per-deck `PitchTempo` cascade handles it on
                // the next `render` call. Ramp is applied inside the
                // cascade via `set_resample_ratio` smoothing — see
                // `audio::pitch_tempo`.
                self.pitch_tempo_mut(deck).set_pitch_semitones(semitones);
            }
            AudioCommandKind::Tempo { deck, ratio, .. } => {
                self.pitch_tempo_mut(deck).set_tempo_ratio(ratio);
            }
            AudioCommandKind::PitchTempoReset { deck } => {
                self.pitch_tempo_mut(deck).reset();
            }
            AudioCommandKind::LoopArm { .. } | AudioCommandKind::LoopDisarm { .. } => {
                // v0.1: loops require buffer playback; no-op.
            }
            AudioCommandKind::DeckLoad { deck, handle } => {
                self.deck_mut(deck).loaded = handle;
            }
            AudioCommandKind::DeckUnload { deck } => {
                let d = self.deck_mut(deck);
                d.loaded = DecodeHandle::NONE;
                d.playing = false;
            }
            AudioCommandKind::ArmHandoff { deck, until_frame } => {
                self.deck_mut(deck).handoff_until_frame = until_frame;
            }
            AudioCommandKind::CancelAfter { .. } => {
                // Pending-command cancellation is enforced on the
                // control thread (it just doesn't push) — no
                // audio-thread state needs touching here. Kept as a
                // distinct variant so the audit log is explicit.
            }
            // ADR-006 — effects chain commands. Each mutates a slot in
            // the deck's `FxBank` array. All paths alloc-free.
            AudioCommandKind::EffectAssign {
                deck,
                slot,
                effect_id,
            } => {
                if let Some(s) = self.effects_mut(deck).get_mut(slot as usize) {
                    if effect_id == EFFECT_NONE {
                        s.clear();
                    } else {
                        s.assign(effect_id);
                    }
                }
            }
            AudioCommandKind::EffectClear { deck, slot } => {
                if let Some(s) = self.effects_mut(deck).get_mut(slot as usize) {
                    s.clear();
                }
            }
            AudioCommandKind::EffectParam {
                deck,
                slot,
                param_id,
                value,
            } => {
                if let Some(s) = self.effects_mut(deck).get_mut(slot as usize) {
                    s.set_param(param_id, value);
                }
            }
            AudioCommandKind::EffectWetDry { deck, slot, value } => {
                if let Some(s) = self.effects_mut(deck).get_mut(slot as usize) {
                    s.wet_dry = value.clamp(0.0, 1.0);
                }
            }
            AudioCommandKind::EffectEnable {
                deck,
                slot,
                enabled,
            } => {
                if let Some(s) = self.effects_mut(deck).get_mut(slot as usize) {
                    s.enabled = enabled;
                }
            }
            AudioCommandKind::SetMasterLimiterEnabled { enabled } => {
                self.limiter.set_enabled(enabled);
            }
            AudioCommandKind::SetMasterLimiterThreshold { threshold_db } => {
                self.limiter.set_threshold_db(threshold_db);
            }
        }
    }

    /// Render `out.len()` mono samples into `out`. **Audio-thread
    /// side.** Alloc-free.
    ///
    /// For each deck:
    /// * If the deck is playing AND has a `DecodeHandle` loaded AND
    ///   the mixer has a decode service wired, pull stereo from the
    ///   decoder and downmix to mono (L+R / 2). Apply per-deck gain.
    /// * Otherwise, fall back to the v0.1 oscillator path. This keeps
    ///   the existing translator + mixer tests (which exercise
    ///   `DeckPlay` without a `DeckLoad`) passing.
    #[inline]
    pub fn render(&mut self, out: &mut [f32]) {
        let sr = self.sample_rate as f32;
        let mut written = 0usize;
        while written < out.len() {
            let chunk = (out.len() - written).min(STEREO_PULL_FRAMES);
            // Pull each deck into its dedicated half of the stereo
            // scratch buffer. Layout:
            //   [0..chunk*2)        = deck A interleaved stereo
            //   [chunk*2..chunk*4)  = deck B interleaved stereo
            let a_end = chunk * 2;
            let b_end = chunk * 4;
            // Borrow-checker dance: do A then B via split_at_mut so
            // each call has its own &mut slice.
            let (a_slice, b_slice) = self.stereo_scratch[..b_end].split_at_mut(a_end);
            let a_pulled = pull_deck(&self.decode, &self.deck_a, a_slice);
            let b_pulled = pull_deck(&self.decode, &self.deck_b, b_slice);

            // Materialize each deck as interleaved stereo into its
            // scratch slice. If the decoder didn't supply data, run
            // the v0.1 oscillator (mono → duplicate into L+R).
            if !a_pulled {
                for i in 0..chunk {
                    let s = render_deck(&mut self.deck_a, sr);
                    a_slice[i * 2] = s;
                    a_slice[i * 2 + 1] = s;
                }
            } else {
                // Apply per-deck gain to the pulled samples in place.
                let g = self.deck_a.gain;
                for s in a_slice[..a_end].iter_mut() {
                    *s *= g;
                }
            }
            if !b_pulled {
                for i in 0..chunk {
                    let s = render_deck(&mut self.deck_b, sr);
                    b_slice[i * 2] = s;
                    b_slice[i * 2 + 1] = s;
                }
            } else {
                let g = self.deck_b.gain;
                for s in b_slice[..(chunk * 2)].iter_mut() {
                    *s *= g;
                }
            }

            // Pitch + tempo cascade (independent controls). When both
            // knobs are at default the cascade takes its `is_bypass()`
            // path — input forwarded verbatim, zero rubato traffic. On
            // any non-default value the cascade runs and we pad /
            // truncate its variable-length output back to `chunk`
            // frames so the downstream effects + crossfade math stays
            // fixed-size. The pad-with-zeros tail is audibly
            // negligible at typical chunk = 256 / sr = 48 kHz (≤5.3 ms
            // grain on tempo ramps); v0.2 will overlap-add to remove it.
            apply_pitch_tempo(
                &mut self.pitch_tempo_a,
                a_slice,
                &mut self.pt_scratch_a,
                chunk * 2,
            );
            apply_pitch_tempo(
                &mut self.pitch_tempo_b,
                &mut b_slice[..(chunk * 2)],
                &mut self.pt_scratch_b,
                chunk * 2,
            );

            // ADR-006 — run each deck's effects chain in slot order.
            // The bank is alloc-free + audio-thread-safe.
            let sr_u = self.sample_rate;
            for slot in self.effects_a.iter_mut() {
                slot.process(&mut a_slice[..a_end], sr_u);
            }
            for slot in self.effects_b.iter_mut() {
                slot.process(&mut b_slice[..(chunk * 2)], sr_u);
            }

            // Downmix to mono + crossfade. Reuses the v0.1 contract
            // (the engine output is mono until a separate stereo PR).
            for i in 0..chunk {
                let a = 0.5 * (a_slice[i * 2] + a_slice[i * 2 + 1]);
                let b = 0.5 * (b_slice[i * 2] + b_slice[i * 2 + 1]);
                let mix = a * (1.0 - self.crossfader) + b * self.crossfader;
                out[written + i] = mix;
            }

            // Master-bus soft-clip limiter. Run **before** both the
            // recorder tee and the cpal output so the live mix +
            // saved `master.wav` are both protected. When the limiter
            // is bypassed the call is a no-op (zero CPU).
            self.limiter
                .process(&mut out[written..written + chunk], self.sample_rate);

            // Tee the limited mix into the recording sink as
            // interleaved stereo (L = R = mono mix) for master.wav.
            if self.recorder.is_some() {
                for i in 0..chunk {
                    let mix = out[written + i];
                    self.rec_scratch[i * 2] = mix;
                    self.rec_scratch[i * 2 + 1] = mix;
                }
            }
            if let Some(rec) = self.recorder.as_mut() {
                rec.push(&self.rec_scratch[..(chunk * 2)]);
            }
            written += chunk;
        }
    }

    /// Read accessor for tests — is the master-bus limiter currently
    /// engaged (not bypassed)?
    pub fn master_limiter_enabled(&self) -> bool {
        self.limiter.enabled()
    }

    /// Read accessor for tests — the current linear ceiling the limiter
    /// is targeting (= `10^(threshold_db/20)`).
    pub fn master_limiter_threshold_linear(&self) -> f32 {
        self.limiter.threshold_linear()
    }

    /// Return a mutable borrow of the per-deck pitch/tempo processor.
    #[inline]
    fn pitch_tempo_mut(&mut self, id: DeckId) -> &mut PitchTempo {
        match id {
            DeckId::A => &mut self.pitch_tempo_a,
            DeckId::B => &mut self.pitch_tempo_b,
        }
    }

    /// Read accessor for tests — current tempo_ratio on a deck.
    pub fn tempo_ratio(&self, id: DeckId) -> f32 {
        match id {
            DeckId::A => self.pitch_tempo_a.tempo_ratio(),
            DeckId::B => self.pitch_tempo_b.tempo_ratio(),
        }
    }

    /// Read accessor for tests — current pitch_semitones on a deck.
    pub fn pitch_semitones(&self, id: DeckId) -> f32 {
        match id {
            DeckId::A => self.pitch_tempo_a.pitch_semitones(),
            DeckId::B => self.pitch_tempo_b.pitch_semitones(),
        }
    }

    /// Return a mutable borrow of the per-deck effects chain.
    #[inline]
    fn effects_mut(&mut self, id: DeckId) -> &mut [FxBank; 3] {
        match id {
            DeckId::A => &mut self.effects_a,
            DeckId::B => &mut self.effects_b,
        }
    }

    /// Read accessor for tests/UI manifest: which effect occupies a
    /// given (deck, slot).
    pub fn effect_id(&self, id: DeckId, slot: u8) -> u32 {
        let bank = match id {
            DeckId::A => &self.effects_a,
            DeckId::B => &self.effects_b,
        };
        bank.get(slot as usize).map(|s| s.effect_id).unwrap_or(0)
    }

    fn deck_mut(&mut self, id: DeckId) -> &mut DeckHot {
        match id {
            DeckId::A => &mut self.deck_a,
            DeckId::B => &mut self.deck_b,
        }
    }

    // Public read accessors for tests + future visualization.
    pub fn crossfader(&self) -> f32 {
        self.crossfader
    }

    pub fn is_playing(&self, deck: DeckId) -> bool {
        match deck {
            DeckId::A => self.deck_a.playing,
            DeckId::B => self.deck_b.playing,
        }
    }

    pub fn handoff_until(&self, deck: DeckId) -> u64 {
        match deck {
            DeckId::A => self.deck_a.handoff_until_frame,
            DeckId::B => self.deck_b.handoff_until_frame,
        }
    }
}

/// Try to pull `dest.len()` (interleaved stereo) samples into `dest`
/// for the given deck. Returns `true` if the decode pipeline supplied
/// the data (i.e., deck is playing + has a loaded handle + a service
/// is wired); `false` if the caller should fall back to the
/// oscillator path.
///
/// `dest` is overwritten regardless — on `false` it's left in
/// whatever state the caller can ignore.
#[inline]
fn pull_deck(decode: &Option<Arc<dyn DecodeService>>, deck: &DeckHot, dest: &mut [f32]) -> bool {
    if !deck.playing || !deck.loaded.is_some() {
        return false;
    }
    let Some(svc) = decode.as_ref() else {
        return false;
    };
    let _ = svc.read(deck.loaded, dest);
    true
}

#[inline]
fn render_deck(d: &mut DeckHot, sr: f32) -> f32 {
    if !d.playing {
        return 0.0;
    }
    let s = d.phase.sin() * d.gain * 0.2; // headroom
    let dphase = std::f32::consts::TAU * d.freq_hz / sr;
    d.phase += dphase;
    if d.phase > std::f32::consts::TAU {
        d.phase -= std::f32::consts::TAU;
    }
    s
}

/// Run the per-deck pitch + tempo cascade in place. `slice` is the
/// deck's interleaved-stereo source samples; on return, `slice[..target_len]`
/// holds the cascade's output, padded with zeros if the cascade
/// produced fewer frames than requested.
///
/// **Audio-thread safe**: alloc-free; the cascade writes into
/// `scratch` and we then copy back into `slice`. No new allocations.
///
/// Implementation notes:
/// * The bypass path inside `PitchTempo::process` short-circuits the
///   first call when both knobs are at default — no copy of more than
///   the original slice.
/// * When the cascade produces fewer samples than `target_len`, the
///   tail is zero-padded. At chunk = 256 / sr = 48kHz this is a ≤5.3 ms
///   silence per block on tempo ramps. v0.2 will replace with a small
///   overlap-add ring; the API surface here is unchanged.
#[inline]
fn apply_pitch_tempo(
    pt: &mut PitchTempo,
    slice: &mut [f32],
    scratch: &mut [f32],
    target_len: usize,
) {
    if pt.is_bypass() {
        // Cascade is a no-op; saves a memcpy.
        return;
    }
    let slice_len = slice.len();
    let n = pt.process(slice, scratch);
    let copy_len = n.min(target_len).min(slice_len);
    slice[..copy_len].copy_from_slice(&scratch[..copy_len]);
    // Zero-pad if the cascade returned fewer samples than the caller
    // asked for. Loud-glitch protection: never leave stale samples.
    let tail_end = target_len.min(slice_len);
    for s in slice[copy_len..tail_end].iter_mut() {
        *s = 0.0;
    }
}

#[inline]
fn db_to_linear(db: f32) -> f32 {
    // 10 ^ (db / 20). Use `exp` since f32::powf can be slow.
    (db * (std::f32::consts::LN_10 / 20.0)).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(kind: AudioCommandKind) -> AudioCommand {
        AudioCommand { at_frame: 0, kind }
    }

    #[test]
    fn silent_when_no_deck_playing() {
        let mut m = AudioMixer::new(48_000);
        let mut buf = [0.0; 64];
        m.render(&mut buf);
        assert!(buf.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn deck_play_emits_nonzero_samples() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::DeckPlay { deck: DeckId::A }));
        // Push crossfader fully to A so we hear the 440Hz oscillator.
        m.apply(cmd(AudioCommandKind::Crossfader {
            target: 0.0,
            ramp_frames: 240,
        }));
        let mut buf = [0.0; 256];
        m.render(&mut buf);
        let energy: f32 = buf.iter().map(|s| s * s).sum();
        assert!(energy > 0.0, "deck A should produce signal");
    }

    #[test]
    fn crossfader_clamps() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::Crossfader {
            target: 2.5,
            ramp_frames: 0,
        }));
        assert!((m.crossfader() - 1.0).abs() < 1e-6);
        m.apply(cmd(AudioCommandKind::Crossfader {
            target: -0.5,
            ramp_frames: 0,
        }));
        assert!((m.crossfader() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn arm_handoff_records_until_frame() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::ArmHandoff {
            deck: DeckId::A,
            until_frame: 96_000,
        }));
        assert_eq!(m.handoff_until(DeckId::A), 96_000);
    }

    #[test]
    fn render_is_alloc_free() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::DeckPlay { deck: DeckId::A }));
        m.apply(cmd(AudioCommandKind::DeckPlay { deck: DeckId::B }));
        let mut buf = [0.0; 1024];
        assert_no_alloc::assert_no_alloc(|| {
            // Apply a handful of commands + render — the entire hot
            // path must be alloc-free per ADR-004.
            m.apply(cmd(AudioCommandKind::Crossfader {
                target: 0.3,
                ramp_frames: 240,
            }));
            m.apply(cmd(AudioCommandKind::EqLow {
                deck: DeckId::A,
                target_db: -6.0,
                ramp_frames: 240,
            }));
            m.render(&mut buf);
        });
    }

    #[test]
    fn apply_tempo_command_caches_value_on_deck_processor() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::Tempo {
            deck: DeckId::B,
            ratio: 1.3,
            ramp_frames: 240,
        }));
        assert!((m.tempo_ratio(DeckId::B) - 1.3).abs() < 1e-6);
        // Other deck unaffected.
        assert!((m.tempo_ratio(DeckId::A) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn apply_pitch_command_caches_value_on_deck_processor() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::Pitch {
            deck: DeckId::A,
            semitones: 5.0,
            ramp_frames: 240,
        }));
        assert!((m.pitch_semitones(DeckId::A) - 5.0).abs() < 1e-6);
    }

    #[test]
    fn pitch_tempo_reset_returns_both_to_defaults_in_mixer() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::Tempo {
            deck: DeckId::A,
            ratio: 0.8,
            ramp_frames: 0,
        }));
        m.apply(cmd(AudioCommandKind::Pitch {
            deck: DeckId::A,
            semitones: 4.0,
            ramp_frames: 0,
        }));
        m.apply(cmd(AudioCommandKind::PitchTempoReset { deck: DeckId::A }));
        assert!((m.tempo_ratio(DeckId::A) - 1.0).abs() < f32::EPSILON);
        assert!(m.pitch_semitones(DeckId::A).abs() < f32::EPSILON);
    }

    /// Latency probe — worst-case `render` for a 1024-frame buffer
    /// with the pitch+tempo cascade active. ADR-004 budget for the
    /// audio thread is ≤ 1ms per render call; this test fails if the
    /// observed worst case across 100 iterations exceeds 1.0ms (≈
    /// 50% margin to the audio-callback budget at 1024 frames @ 48
    /// kHz ≈ 21.3 ms wall-clock).
    ///
    /// Run with `cargo test --release` to measure realistic numbers;
    /// in debug builds the bound is relaxed to 5ms.
    #[test]
    fn render_1024_frame_pitch_tempo_active_meets_latency_budget() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::DeckPlay { deck: DeckId::A }));
        m.apply(cmd(AudioCommandKind::DeckPlay { deck: DeckId::B }));
        m.apply(cmd(AudioCommandKind::Tempo {
            deck: DeckId::A,
            ratio: 1.07,
            ramp_frames: 0,
        }));
        m.apply(cmd(AudioCommandKind::Pitch {
            deck: DeckId::A,
            semitones: 2.0,
            ramp_frames: 0,
        }));
        m.apply(cmd(AudioCommandKind::Tempo {
            deck: DeckId::B,
            ratio: 0.93,
            ramp_frames: 0,
        }));
        m.apply(cmd(AudioCommandKind::Pitch {
            deck: DeckId::B,
            semitones: -3.0,
            ramp_frames: 0,
        }));
        let mut buf = [0.0_f32; 1024];
        // Prime — first call fills rubato's polynomial state.
        m.render(&mut buf);
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..100 {
            let t = std::time::Instant::now();
            m.render(&mut buf);
            let dt = t.elapsed();
            if dt > worst {
                worst = dt;
            }
        }
        let budget = if cfg!(debug_assertions) {
            std::time::Duration::from_millis(5)
        } else {
            std::time::Duration::from_millis(1)
        };
        // Print so the test can be used as a quick probe via
        // `cargo test ... -- --nocapture`. The actual gate is the
        // assertion below.
        eprintln!(
            "[latency] 1024-frame render w/ pitch+tempo active: worst = {worst:?}, budget = {budget:?}"
        );
        assert!(
            worst <= budget,
            "render exceeded latency budget: worst {worst:?} > {budget:?}"
        );
    }

    #[test]
    fn render_with_non_default_tempo_still_alloc_free() {
        // ADR-004 — the active pitch/tempo cascade must remain
        // alloc-free on the audio thread. This is the strongest test
        // because rubato's `process_into_buffer` is non-trivial; if it
        // ever heap-allocs internally the assert_no_alloc gate trips.
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::DeckPlay { deck: DeckId::A }));
        m.apply(cmd(AudioCommandKind::Tempo {
            deck: DeckId::A,
            ratio: 1.2,
            ramp_frames: 0,
        }));
        m.apply(cmd(AudioCommandKind::Pitch {
            deck: DeckId::A,
            semitones: 3.0,
            ramp_frames: 0,
        }));
        // Prime — first render fills rubato's polynomial buffer.
        let mut buf = [0.0_f32; 1024];
        m.render(&mut buf);
        assert_no_alloc::assert_no_alloc(|| {
            m.render(&mut buf);
        });
    }

    #[test]
    fn master_limiter_enabled_by_default_on_new_mixer() {
        // Safety-first: limiter ON the moment the mixer is constructed.
        // Catches a regression of `MasterLimiter::new` defaulting to
        // disabled (would silently un-protect the master bus).
        let m = AudioMixer::new(48_000);
        assert!(m.master_limiter_enabled());
        // Default threshold ≈ 0.944 linear (-0.5 dB).
        let thr = m.master_limiter_threshold_linear();
        assert!((thr - 10_f32.powf(-0.5 / 20.0)).abs() < 1e-4);
    }

    #[test]
    fn set_master_limiter_command_updates_mixer_state() {
        let mut m = AudioMixer::new(48_000);
        m.apply(cmd(AudioCommandKind::SetMasterLimiterEnabled {
            enabled: false,
        }));
        assert!(!m.master_limiter_enabled());
        m.apply(cmd(AudioCommandKind::SetMasterLimiterThreshold {
            threshold_db: -12.0,
        }));
        let thr = m.master_limiter_threshold_linear();
        assert!(
            (thr - 10_f32.powf(-12.0 / 20.0)).abs() < 1e-4,
            "threshold_linear should reflect SetMasterLimiterThreshold event, got {thr}",
        );
    }
}
