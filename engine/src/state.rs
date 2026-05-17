//! Event-sourced engine state (ADR-003).
//!
//! `EngineState` is the fold of an event log. Every UI/MIDI/copilot input
//! becomes an `Event`; the reducer applies it deterministically. No shared
//! mutable state across threads — the audio thread reads a lock-free
//! snapshot of `EngineState` and renders.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckId {
    A,
    B,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EqBand {
    Low,
    Mid,
    High,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EventSource {
    Ui,
    Midi { device: String, mapping: String },
    Copilot,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TrackRef {
    pub id: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EventKind {
    DeckLoad { deck: DeckId, track: TrackRef },
    DeckPlay { deck: DeckId },
    DeckPause { deck: DeckId },
    DeckCue { deck: DeckId, position_ms: u64 },
    Crossfader { value: f32 },
    EqAdjust { deck: DeckId, band: EqBand, value_db: f32 },
    HotCueSet { deck: DeckId, slot: u8, position_ms: u64 },
    HotCueTrigger { deck: DeckId, slot: u8 },
    LoopIn { deck: DeckId },
    LoopOut { deck: DeckId },
    LoopExit { deck: DeckId },
    PitchBend { deck: DeckId, semitones: f32 },
    CopilotEngage { deck: DeckId },
    CopilotDisengage { deck: DeckId },
    TakeOver { deck: DeckId },
    SessionStart,
    SessionEnd,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Event {
    pub id: u64,
    pub ts_micros: i64,
    pub source: EventSource,
    pub kind: EventKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Deck {
    pub loaded: Option<TrackRef>,
    pub playing: bool,
    pub position_ms: u64,
    pub pitch_semitones: f32,
    pub eq_low_db: f32,
    pub eq_mid_db: f32,
    pub eq_high_db: f32,
    pub loop_in_ms: Option<u64>,
    pub loop_out_ms: Option<u64>,
    pub loop_active: bool,
    pub hot_cues: [Option<u64>; 8],
    pub copilot_engaged: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EngineState {
    pub deck_a: Deck,
    pub deck_b: Deck,
    pub crossfader: f32, // 0.0 = full A, 1.0 = full B
    pub master_volume_db: f32,
    pub session_active: bool,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            deck_a: Deck::default(),
            deck_b: Deck::default(),
            crossfader: 0.5,
            master_volume_db: 0.0,
            session_active: false,
        }
    }
}

impl EngineState {
    /// Apply an event to produce a new state. Pure function — never mutates self.
    pub fn apply(&self, ev: &Event) -> Self {
        let mut next = self.clone();
        match &ev.kind {
            EventKind::SessionStart => next.session_active = true,
            EventKind::SessionEnd => next.session_active = false,
            EventKind::DeckLoad { deck: id, track } => {
                let d = next.deck_mut(*id);
                d.loaded = Some(track.clone());
                d.position_ms = 0;
                d.playing = false;
            }
            EventKind::DeckPlay { deck: id } => {
                next.deck_mut(*id).playing = true;
            }
            EventKind::DeckPause { deck: id } => {
                next.deck_mut(*id).playing = false;
            }
            EventKind::DeckCue { deck: id, position_ms } => {
                next.deck_mut(*id).position_ms = *position_ms;
            }
            EventKind::Crossfader { value } => {
                next.crossfader = value.clamp(0.0, 1.0);
            }
            EventKind::EqAdjust { deck: id, band, value_db } => {
                let clamped = value_db.clamp(-26.0, 6.0); // pro convention
                let d = next.deck_mut(*id);
                match band {
                    EqBand::Low => d.eq_low_db = clamped,
                    EqBand::Mid => d.eq_mid_db = clamped,
                    EqBand::High => d.eq_high_db = clamped,
                }
            }
            EventKind::HotCueSet { deck: id, slot, position_ms } => {
                if (*slot as usize) < 8 {
                    next.deck_mut(*id).hot_cues[*slot as usize] = Some(*position_ms);
                }
            }
            EventKind::HotCueTrigger { deck: id, slot } => {
                if (*slot as usize) < 8 {
                    if let Some(pos) = next.deck(*id).hot_cues[*slot as usize] {
                        next.deck_mut(*id).position_ms = pos;
                    }
                }
            }
            EventKind::LoopIn { deck: id } => {
                let pos = next.deck(*id).position_ms;
                next.deck_mut(*id).loop_in_ms = Some(pos);
            }
            EventKind::LoopOut { deck: id } => {
                let pos = next.deck(*id).position_ms;
                let had_in = next.deck(*id).loop_in_ms.is_some();
                let d = next.deck_mut(*id);
                d.loop_out_ms = Some(pos);
                d.loop_active = had_in;
            }
            EventKind::LoopExit { deck: id } => {
                let d = next.deck_mut(*id);
                d.loop_in_ms = None;
                d.loop_out_ms = None;
                d.loop_active = false;
            }
            EventKind::PitchBend { deck: id, semitones } => {
                next.deck_mut(*id).pitch_semitones = semitones.clamp(-12.0, 12.0);
            }
            EventKind::CopilotEngage { deck: id } => {
                next.deck_mut(*id).copilot_engaged = true;
            }
            EventKind::CopilotDisengage { deck: id } => {
                next.deck_mut(*id).copilot_engaged = false;
            }
            EventKind::TakeOver { deck: id } => {
                // User pre-empts copilot — disengage immediately. Audio
                // crossfader / EQ stays as the copilot left it; further
                // events are user-driven.
                next.deck_mut(*id).copilot_engaged = false;
            }
        }
        next
    }

    fn deck(&self, id: DeckId) -> &Deck {
        match id {
            DeckId::A => &self.deck_a,
            DeckId::B => &self.deck_b,
        }
    }

    fn deck_mut(&mut self, id: DeckId) -> &mut Deck {
        match id {
            DeckId::A => &mut self.deck_a,
            DeckId::B => &mut self.deck_b,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: u64, kind: EventKind) -> Event {
        Event {
            id,
            ts_micros: 0,
            source: EventSource::Ui,
            kind,
        }
    }

    #[test]
    fn deck_play_then_pause() {
        let s = EngineState::default();
        let s = s.apply(&ev(1, EventKind::DeckPlay { deck: DeckId::A }));
        assert!(s.deck_a.playing);
        let s = s.apply(&ev(2, EventKind::DeckPause { deck: DeckId::A }));
        assert!(!s.deck_a.playing);
    }

    #[test]
    fn crossfader_clamps() {
        let s = EngineState::default();
        let s = s.apply(&ev(1, EventKind::Crossfader { value: 2.5 }));
        assert_eq!(s.crossfader, 1.0);
        let s = s.apply(&ev(2, EventKind::Crossfader { value: -0.5 }));
        assert_eq!(s.crossfader, 0.0);
    }

    #[test]
    fn eq_clamps_to_pro_range() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::EqAdjust {
                deck: DeckId::A,
                band: EqBand::Low,
                value_db: -50.0,
            },
        ));
        assert_eq!(s.deck_a.eq_low_db, -26.0);
        let s = s.apply(&ev(
            2,
            EventKind::EqAdjust {
                deck: DeckId::B,
                band: EqBand::High,
                value_db: 99.0,
            },
        ));
        assert_eq!(s.deck_b.eq_high_db, 6.0);
    }

    #[test]
    fn hot_cue_set_then_trigger_seeks() {
        let s = EngineState::default();
        // Place deck A at 60000ms first
        let s = s.apply(&ev(
            1,
            EventKind::DeckCue {
                deck: DeckId::A,
                position_ms: 60000,
            },
        ));
        // Save hot cue to slot 0
        let s = s.apply(&ev(
            2,
            EventKind::HotCueSet {
                deck: DeckId::A,
                slot: 0,
                position_ms: 60000,
            },
        ));
        // Move somewhere else
        let s = s.apply(&ev(
            3,
            EventKind::DeckCue {
                deck: DeckId::A,
                position_ms: 90000,
            },
        ));
        assert_eq!(s.deck_a.position_ms, 90000);
        // Trigger hot cue 0 — should jump back to 60000
        let s = s.apply(&ev(4, EventKind::HotCueTrigger { deck: DeckId::A, slot: 0 }));
        assert_eq!(s.deck_a.position_ms, 60000);
    }

    #[test]
    fn takeover_disengages_copilot() {
        let s = EngineState::default();
        let s = s.apply(&ev(1, EventKind::CopilotEngage { deck: DeckId::A }));
        assert!(s.deck_a.copilot_engaged);
        let s = s.apply(&ev(2, EventKind::TakeOver { deck: DeckId::A }));
        assert!(!s.deck_a.copilot_engaged);
    }

    #[test]
    fn apply_is_pure() {
        let s = EngineState::default();
        let _ = s.apply(&ev(1, EventKind::DeckPlay { deck: DeckId::A }));
        // Original must be untouched (clone semantics).
        assert!(!s.deck_a.playing);
    }
}
