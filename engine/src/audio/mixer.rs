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

use crate::audio::command::{AudioCommand, AudioCommandKind, BufferId};
use crate::state::DeckId;

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
    /// Currently loaded buffer id (informational — not yet used to
    /// render in v0.1 since the oscillator is the sound source). Real
    /// playback lands in a later PR.
    loaded_buffer: Option<BufferId>,
}

impl DeckHot {
    const fn new(freq_hz: f32) -> Self {
        Self {
            playing: false,
            phase: 0.0,
            freq_hz,
            gain: 1.0,
            handoff_until_frame: 0,
            loaded_buffer: None,
        }
    }
}

/// Audio-thread mixing state. Lives behind the cpal callback. Never
/// allocates after construction.
pub struct AudioMixer {
    sample_rate: u32,
    crossfader: f32,
    deck_a: DeckHot,
    deck_b: DeckHot,
}

impl AudioMixer {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            crossfader: 0.5,
            deck_a: DeckHot::new(440.0),
            deck_b: DeckHot::new(220.0),
        }
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
            AudioCommandKind::DeckLoadBuffer { deck, buffer_id } => {
                self.deck_mut(deck).loaded_buffer = Some(buffer_id);
            }
            AudioCommandKind::DeckUnload { deck } => {
                let d = self.deck_mut(deck);
                d.loaded_buffer = None;
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
        }
    }

    /// Render `frames` mono samples into `out`. **Audio-thread side.**
    /// Alloc-free.
    #[inline]
    pub fn render(&mut self, out: &mut [f32]) {
        let sr = self.sample_rate as f32;
        for sample in out.iter_mut() {
            let a = render_deck(&mut self.deck_a, sr);
            let b = render_deck(&mut self.deck_b, sr);
            // Crossfader: 0 = full A, 1 = full B (matches `EngineState`).
            let mix = a * (1.0 - self.crossfader) + b * self.crossfader;
            *sample = mix;
        }
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
