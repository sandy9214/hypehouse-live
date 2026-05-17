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
    DeckLoad {
        deck: DeckId,
        track: TrackRef,
        /// Pre-analyzed BPM + beat-grid anchor; sourced from the copilot
        /// analyzer service (HypeHouse v1 carry-over). Required for
        /// beat-matching. ADR-002 council review (Codex).
        bpm: f32,
        beat_grid_anchor_ms: u64,
    },
    /// ADR review Groq: explicit DeckUnload so the engine can free buffers
    /// and clear state cleanly (vs. relying on DeckLoad implicit replace).
    DeckUnload {
        deck: DeckId,
    },
    DeckPlay {
        deck: DeckId,
    },
    DeckPause {
        deck: DeckId,
    },
    DeckCue {
        deck: DeckId,
        position_ms: u64,
    },
    Crossfader {
        value: f32,
    },
    EqAdjust {
        deck: DeckId,
        band: EqBand,
        value_db: f32,
    },
    HotCueSet {
        deck: DeckId,
        slot: u8,
        position_ms: u64,
    },
    HotCueTrigger {
        deck: DeckId,
        slot: u8,
    },
    LoopIn {
        deck: DeckId,
    },
    LoopOut {
        deck: DeckId,
    },
    LoopExit {
        deck: DeckId,
    },
    PitchBend {
        deck: DeckId,
        semitones: f32,
    },
    /// Phase nudge — apply manual offset to deck's beat grid for sync (ADR-007).
    PhaseNudge {
        deck: DeckId,
        delta_ms: i32,
    },
    /// Effects (ADR-006).
    EffectAssign {
        deck: DeckId,
        slot: u8,
        effect_id: u32,
    },
    EffectClear {
        deck: DeckId,
        slot: u8,
    },
    EffectParam {
        deck: DeckId,
        slot: u8,
        param: String,
        value: f32,
    },
    EffectWetDry {
        deck: DeckId,
        slot: u8,
        value: f32,
    },
    EffectEnable {
        deck: DeckId,
        slot: u8,
        enabled: bool,
    },
    CopilotEngage {
        deck: DeckId,
    },
    CopilotDisengage {
        deck: DeckId,
    },
    /// User pre-empts the AI mid-transition. ADR-005 defines a bounded
    /// 1-bar handoff envelope; the audio thread continues AI automation
    /// for that window while ramping user inputs in. The control thread
    /// computes `handoff_until_frame` from current engine clock + the
    /// deck's beat_period_ms (4 beats = 1 bar at 4/4) and stamps it on
    /// the event before applying — reducer is then pure.
    TakeOver {
        deck: DeckId,
        handoff_until_frame: u64,
    },
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
    /// Council ADR-002 review (Codex): live mixing needs beatgrid + tempo
    /// + phase on the deck or beat-matching can't be reasoned about.
    pub bpm: f32,
    pub beat_grid_anchor_ms: u64, // ms of beat 0 in the track
    pub beat_period_ms: f32,      // milliseconds per beat (= 60_000 / bpm)
    pub phase_offset_ms: i32,     // user-applied phase nudge (±)
    /// Effects chain (ADR-006). 3 slots per deck.
    pub effects: [EffectSlot; 3],
    /// Co-pilot takeover handoff window end (ADR-005). 0 = no handoff active.
    pub handoff_until_frame: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EffectSlot {
    /// Effect-registry ID. 0 = empty slot.
    pub effect_id: u32,
    /// Param name → value. BTreeMap for deterministic ordering across forks.
    pub params: std::collections::BTreeMap<String, f32>,
    /// 0.0 = dry, 1.0 = full wet.
    pub wet_dry: f32,
    pub enabled: bool,
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
            EventKind::DeckLoad {
                deck: id,
                track,
                bpm,
                beat_grid_anchor_ms,
            } => {
                let d = next.deck_mut(*id);
                d.loaded = Some(track.clone());
                d.position_ms = 0;
                d.playing = false;
                let safe_bpm = if bpm.is_finite() && *bpm > 0.0 {
                    *bpm
                } else {
                    120.0
                };
                d.bpm = safe_bpm;
                d.beat_grid_anchor_ms = *beat_grid_anchor_ms;
                d.beat_period_ms = 60_000.0 / safe_bpm;
                d.phase_offset_ms = 0;
            }
            EventKind::DeckUnload { deck: id } => {
                *next.deck_mut(*id) = Deck::default();
            }
            EventKind::PhaseNudge { deck: id, delta_ms } => {
                let d = next.deck_mut(*id);
                d.phase_offset_ms = d.phase_offset_ms.saturating_add(*delta_ms);
            }
            EventKind::EffectAssign {
                deck: id,
                slot,
                effect_id,
            } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    *s = EffectSlot {
                        effect_id: *effect_id,
                        params: Default::default(),
                        wet_dry: 0.5,
                        enabled: true,
                    };
                }
            }
            EventKind::EffectClear { deck: id, slot } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    *s = EffectSlot::default();
                }
            }
            EventKind::EffectParam {
                deck: id,
                slot,
                param,
                value,
            } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    s.params.insert(param.clone(), *value);
                }
            }
            EventKind::EffectWetDry {
                deck: id,
                slot,
                value,
            } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    s.wet_dry = value.clamp(0.0, 1.0);
                }
            }
            EventKind::EffectEnable {
                deck: id,
                slot,
                enabled,
            } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    s.enabled = *enabled;
                }
            }
            EventKind::DeckPlay { deck: id } => {
                next.deck_mut(*id).playing = true;
            }
            EventKind::DeckPause { deck: id } => {
                next.deck_mut(*id).playing = false;
            }
            EventKind::DeckCue {
                deck: id,
                position_ms,
            } => {
                next.deck_mut(*id).position_ms = *position_ms;
            }
            EventKind::Crossfader { value } => {
                next.crossfader = value.clamp(0.0, 1.0);
            }
            EventKind::EqAdjust {
                deck: id,
                band,
                value_db,
            } => {
                let clamped = value_db.clamp(-26.0, 6.0); // pro convention
                let d = next.deck_mut(*id);
                match band {
                    EqBand::Low => d.eq_low_db = clamped,
                    EqBand::Mid => d.eq_mid_db = clamped,
                    EqBand::High => d.eq_high_db = clamped,
                }
            }
            EventKind::HotCueSet {
                deck: id,
                slot,
                position_ms,
            } => {
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
            EventKind::PitchBend {
                deck: id,
                semitones,
            } => {
                next.deck_mut(*id).pitch_semitones = semitones.clamp(-12.0, 12.0);
            }
            EventKind::CopilotEngage { deck: id } => {
                next.deck_mut(*id).copilot_engaged = true;
            }
            EventKind::CopilotDisengage { deck: id } => {
                next.deck_mut(*id).copilot_engaged = false;
            }
            EventKind::TakeOver {
                deck: id,
                handoff_until_frame,
            } => {
                // User pre-empts copilot — disengage immediately + set
                // the 1-bar handoff window end per ADR-005. Audio
                // thread continues AI automation through this frame
                // while user inputs cross-fade in. Reducer stores the
                // value computed by the control thread; pure function.
                let d = next.deck_mut(*id);
                d.copilot_engaged = false;
                d.handoff_until_frame = *handoff_until_frame;
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
        let s = s.apply(&ev(
            4,
            EventKind::HotCueTrigger {
                deck: DeckId::A,
                slot: 0,
            },
        ));
        assert_eq!(s.deck_a.position_ms, 60000);
    }

    #[test]
    fn takeover_disengages_copilot_and_arms_handoff() {
        let s = EngineState::default();
        let s = s.apply(&ev(1, EventKind::CopilotEngage { deck: DeckId::A }));
        assert!(s.deck_a.copilot_engaged);
        let s = s.apply(&ev(
            2,
            EventKind::TakeOver {
                deck: DeckId::A,
                handoff_until_frame: 96_000, // ~2s at 48kHz = ~1 bar at 120 BPM
            },
        ));
        assert!(!s.deck_a.copilot_engaged);
        assert_eq!(s.deck_a.handoff_until_frame, 96_000);
    }

    #[test]
    fn apply_is_pure() {
        let s = EngineState::default();
        let _ = s.apply(&ev(1, EventKind::DeckPlay { deck: DeckId::A }));
        // Original must be untouched (clone semantics).
        assert!(!s.deck_a.playing);
    }

    #[test]
    fn deck_load_sets_beatgrid_and_tempo() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t1".into(),
                    path: "/tracks/x.mp3".into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 220,
            },
        ));
        assert_eq!(s.deck_a.bpm, 128.0);
        assert_eq!(s.deck_a.beat_grid_anchor_ms, 220);
        assert!((s.deck_a.beat_period_ms - (60_000.0 / 128.0)).abs() < 0.001);
    }

    #[test]
    fn deck_load_clamps_invalid_bpm_to_default() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t".into(),
                    path: "/p".into(),
                },
                bpm: f32::NAN,
                beat_grid_anchor_ms: 0,
            },
        ));
        assert_eq!(s.deck_a.bpm, 120.0);
    }

    #[test]
    fn deck_unload_clears_state() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t".into(),
                    path: "/p".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
            },
        ));
        let s = s.apply(&ev(2, EventKind::DeckPlay { deck: DeckId::A }));
        assert!(s.deck_a.loaded.is_some());
        let s = s.apply(&ev(3, EventKind::DeckUnload { deck: DeckId::A }));
        assert!(s.deck_a.loaded.is_none());
        assert!(!s.deck_a.playing);
    }

    #[test]
    fn phase_nudge_accumulates() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::PhaseNudge {
                deck: DeckId::A,
                delta_ms: 10,
            },
        ));
        let s = s.apply(&ev(
            2,
            EventKind::PhaseNudge {
                deck: DeckId::A,
                delta_ms: -3,
            },
        ));
        assert_eq!(s.deck_a.phase_offset_ms, 7);
    }

    #[test]
    fn effect_assign_and_param_clamp_wet_dry() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::EffectAssign {
                deck: DeckId::A,
                slot: 0,
                effect_id: 1,
            },
        ));
        let s = s.apply(&ev(
            2,
            EventKind::EffectParam {
                deck: DeckId::A,
                slot: 0,
                param: "cutoff_hz".into(),
                value: 500.0,
            },
        ));
        let s = s.apply(&ev(
            3,
            EventKind::EffectWetDry {
                deck: DeckId::A,
                slot: 0,
                value: 2.5,
            },
        ));
        assert_eq!(s.deck_a.effects[0].effect_id, 1);
        assert_eq!(s.deck_a.effects[0].params.get("cutoff_hz"), Some(&500.0));
        assert_eq!(s.deck_a.effects[0].wet_dry, 1.0); // clamped
    }
}
