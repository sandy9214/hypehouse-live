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
}

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
        }
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
            AudioCommandKind::Pitch { .. } => {
                // v0.1: pitch shifting requires the buffer playback
                // path; no-op until real audio lands.
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
            written += chunk;
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
}
