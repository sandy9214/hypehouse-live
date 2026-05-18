//! Event-sourced engine state (ADR-003).
//!
//! `EngineState` is the fold of an event log. Every UI/MIDI/copilot input
//! becomes an `Event`; the reducer applies it deterministically. No shared
//! mutable state across threads — the audio thread reads a lock-free
//! snapshot of `EngineState` and renders.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Inline capacity for the per-deck downbeat grid. A 4/4 track at 120 BPM
/// in 4-bar phrases generates ~1 downbeat every 8 beats; a 5-minute set
/// (300s) at 120 BPM has ~37 downbeats. We round up to 64 so most pop /
/// EDM tracks fit on the stack — heap spill is only paid by edge-case
/// long tracks (≥10 min at fast tempo). Tracks with more downbeats are
/// truncated by the reducer; see `EventKind::DeckLoad` handling.
pub const DOWNBEATS_INLINE_CAPACITY: usize = 64;

/// Per-deck downbeat grid (millisecond positions inside the track). u32
/// ceiling = ~71 minutes, more than any sane DJ track. Storing u32 keeps
/// the SmallVec small (256B inline vs 512B for u64) which matters because
/// `Deck` is cloned by the pure reducer on every event.
pub type DownbeatGrid = SmallVec<[u32; DOWNBEATS_INLINE_CAPACITY]>;

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
        /// Downbeat (bar-start) positions in milliseconds, sourced from
        /// the copilot's madmom DBNDownBeatTracker pass. Optional so old
        /// payloads (before beat-grid analysis landed) still deserialize
        /// — they just leave the deck with an empty downbeat grid and
        /// phrase-aligned transitions fall back to bar-grid math derived
        /// from `beat_grid_anchor_ms` + `beat_period_ms × 4`.
        ///
        /// Truncated to `DOWNBEATS_INLINE_CAPACITY` (64) entries inside
        /// the reducer — see :meth:`EngineState::apply`. Most tracks fit
        /// comfortably; the cap protects the inline `SmallVec` budget.
        #[serde(default)]
        downbeats_ms: Vec<u32>,
        /// 8-slot hot-cue grid sourced from the copilot's library
        /// (added in the hot-cue persistence PR). `Some(ms)` = saved
        /// cue position relative to track start, `None` = empty slot.
        /// Optional so pre-this-PR `DeckLoad` payloads still
        /// deserialize via serde's default (all None) — they just
        /// load a track with no preset hot-cues. The shape mirrors
        /// `Deck::hot_cues` exactly so the reducer's assignment is a
        /// direct copy.
        #[serde(default = "default_hot_cues")]
        hot_cues: [Option<u64>; 8],
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
    /// Pure pitch shift in semitones (key change). **Tempo unchanged**
    /// — drives only the per-deck pitch resampler stage. Use
    /// `TempoBend` for tempo control. Clamped to ±12 by the reducer.
    PitchBend {
        deck: DeckId,
        semitones: f32,
    },
    /// Independent tempo control — ratio of playback speed. 1.0 =
    /// normal, < 1 = slower, > 1 = faster. Pitch is preserved by the
    /// per-deck `PitchTempo` cascade. Reducer clamps to
    /// `[audio::MIN_TEMPO_RATIO, audio::MAX_TEMPO_RATIO]` and rejects
    /// non-finite inputs (treats them as 1.0). Companion to
    /// `PitchBend`; the two knobs are independent in the public API
    /// (v0.1 cascade implementation has a documented limitation — see
    /// `engine/src/audio/pitch_tempo.rs`).
    TempoBend {
        deck: DeckId,
        ratio: f32,
    },
    /// Convenience event — reset both `pitch_semitones` and
    /// `tempo_ratio` on a deck to their defaults (0.0 / 1.0). Emits
    /// `AudioCommandKind::PitchTempoReset` so the audio thread also
    /// resets the rubato cascade state.
    PitchTempoReset {
        deck: DeckId,
    },
    /// Phase nudge — apply manual offset to deck's beat grid for sync (ADR-007).
    PhaseNudge {
        deck: DeckId,
        delta_ms: i32,
    },
    /// Set the session master BPM (ADR-007 §v0.1). Drives the MIDI clock
    /// OUT tick thread + any future Ableton Link master. Non-finite or
    /// non-positive values are clamped to the previous master BPM by
    /// the reducer (no-op apply).
    SetMasterBpm {
        bpm: f32,
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
    /// Toggle the master-bus soft-clip limiter (default `true` for
    /// safety — both the live mix and the recorded `master.wav` are
    /// kept inside ±1.0 when both decks are hot + effects are active).
    /// See [`crate::audio::limiter`] for the algorithm.
    SetMasterLimiterEnabled {
        enabled: bool,
    },
    /// Set the master-bus limiter's threshold in dB. Reducer clamps to
    /// `[audio::MASTER_LIMITER_MIN_THRESHOLD_DB, audio::MASTER_LIMITER_MAX_THRESHOLD_DB]`
    /// (= `[-24.0, 0.0]`). Non-finite inputs fall back to the default
    /// (-0.5 dB) per `audio::clamp_master_limiter_threshold_db`.
    SetMasterLimiterThreshold {
        threshold_db: f32,
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Deck {
    pub loaded: Option<TrackRef>,
    pub playing: bool,
    pub position_ms: u64,
    /// **Pure pitch shift** in semitones — independent of tempo (post
    /// the pitch/tempo-independent PR). 0.0 = original key. Clamped to
    /// ±12. Drives stage 1 of the per-deck rubato cascade in
    /// `audio::pitch_tempo`.
    pub pitch_semitones: f32,
    /// **Tempo ratio** — playback-speed multiplier independent of
    /// pitch. 1.0 = normal speed. Clamped to
    /// `[audio::MIN_TEMPO_RATIO, audio::MAX_TEMPO_RATIO]` (default
    /// 0.5..2.0). Drives stage 2 of the per-deck rubato cascade.
    #[serde(default = "default_tempo_ratio")]
    pub tempo_ratio: f32,
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
    /// Downbeat (bar-start) positions in ms, populated from the copilot
    /// analyzer's madmom pass on `DeckLoad`. Inline capacity =
    /// `DOWNBEATS_INLINE_CAPACITY` (64) — sufficient for most 3-5 minute
    /// tracks at common tempos. Tracks with more downbeats are truncated
    /// to the first 64 in the reducer (see `EngineState::apply`); the
    /// truncation is intentional and documented on the field rather than
    /// dropped silently — callers should not see runtime cliffs at the
    /// boundary.
    #[serde(default, skip_serializing_if = "SmallVec::is_empty")]
    pub downbeats: DownbeatGrid,
    /// Effects chain (ADR-006). 3 slots per deck.
    pub effects: [EffectSlot; 3],
    /// Co-pilot takeover handoff window end (ADR-005). 0 = no handoff active.
    pub handoff_until_frame: u64,
}

impl Default for Deck {
    fn default() -> Self {
        // Hand-written `Default` because `tempo_ratio` defaults to 1.0
        // (not 0.0). Every other field uses its type's natural default;
        // capturing them explicitly avoids drift if a field is added
        // upstream without a matching default-handler edit here.
        Self {
            loaded: None,
            playing: false,
            position_ms: 0,
            pitch_semitones: 0.0,
            tempo_ratio: default_tempo_ratio(),
            eq_low_db: 0.0,
            eq_mid_db: 0.0,
            eq_high_db: 0.0,
            loop_in_ms: None,
            loop_out_ms: None,
            loop_active: false,
            hot_cues: [None; 8],
            copilot_engaged: false,
            bpm: 0.0,
            beat_grid_anchor_ms: 0,
            beat_period_ms: 0.0,
            phase_offset_ms: 0,
            downbeats: DownbeatGrid::new(),
            effects: Default::default(),
            handoff_until_frame: 0,
        }
    }
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
    /// Session master BPM (ADR-007 §v0.1). Drives MIDI clock OUT period.
    /// Default 120.0, updated by `EventKind::SetMasterBpm`.
    #[serde(default = "default_master_bpm")]
    pub master_bpm: f32,
    /// Master-bus soft-clip limiter — toggle. Default `true` so the
    /// live mix + recorded `master.wav` are protected against clipping
    /// from the moment the engine starts. See [`crate::audio::limiter`].
    #[serde(default = "default_master_limiter_enabled")]
    pub master_limiter_enabled: bool,
    /// Master-bus soft-clip limiter — threshold in dB. Default `-0.5`
    /// (linear ≈ 0.944). Reducer clamps incoming values to the
    /// `[-24.0, 0.0]` window via
    /// `audio::clamp_master_limiter_threshold_db`.
    #[serde(default = "default_master_limiter_threshold_db")]
    pub master_limiter_threshold_db: f32,
}

fn default_master_bpm() -> f32 {
    120.0
}

fn default_master_limiter_enabled() -> bool {
    true
}

fn default_master_limiter_threshold_db() -> f32 {
    crate::audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB
}

/// Serde default for `Deck::tempo_ratio` — 1.0 = original playback
/// speed. Lives next to `default_master_bpm` so both serde defaults are
/// findable together.
fn default_tempo_ratio() -> f32 {
    1.0
}

/// Serde default for `EventKind::DeckLoad.hot_cues` — 8 empty slots.
/// Used when an old payload (pre hot-cue persistence PR) omits the
/// field; the reducer then leaves the deck's cue grid untouched of
/// new entries. Tuple-style `[None; 8]` would require `Copy` semantics
/// the function form sidesteps cleanly.
fn default_hot_cues() -> [Option<u64>; 8] {
    [None; 8]
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            deck_a: Deck::default(),
            deck_b: Deck::default(),
            crossfader: 0.5,
            master_volume_db: 0.0,
            session_active: false,
            master_bpm: default_master_bpm(),
            master_limiter_enabled: default_master_limiter_enabled(),
            master_limiter_threshold_db: default_master_limiter_threshold_db(),
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
                downbeats_ms,
                hot_cues,
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
                // Truncate to inline capacity. SmallVec::from_iter spills to
                // heap only when input > inline cap, but we cap explicitly so
                // the audio-thread snapshot is bounded regardless of input
                // size — a malicious or buggy copilot can't blow up `Deck`
                // size via a giant downbeats array.
                let take = downbeats_ms.len().min(DOWNBEATS_INLINE_CAPACITY);
                d.downbeats = DownbeatGrid::from_slice(&downbeats_ms[..take]);
                // Replace the per-deck hot-cue grid wholesale. Loading
                // a new track always overwrites any in-memory cues
                // (matches the prior "DeckLoad replaces deck state"
                // contract); pre-PR payloads come in with all-None
                // via the serde default and so behave exactly like
                // before.
                d.hot_cues = *hot_cues;
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
                next.deck_mut(*id).pitch_semitones =
                    crate::audio::clamp_pitch_semitones(*semitones);
            }
            EventKind::TempoBend { deck: id, ratio } => {
                // Use the audio module's clamp so the reducer and the
                // audio-thread `PitchTempo::set_tempo_ratio` apply the
                // exact same range — no risk of the state log holding
                // a value the audio path silently re-clamps differently.
                next.deck_mut(*id).tempo_ratio = crate::audio::clamp_tempo_ratio(*ratio);
            }
            EventKind::PitchTempoReset { deck: id } => {
                let d = next.deck_mut(*id);
                d.pitch_semitones = 0.0;
                d.tempo_ratio = default_tempo_ratio();
            }
            EventKind::CopilotEngage { deck: id } => {
                next.deck_mut(*id).copilot_engaged = true;
            }
            EventKind::CopilotDisengage { deck: id } => {
                next.deck_mut(*id).copilot_engaged = false;
            }
            EventKind::SetMasterBpm { bpm } => {
                if bpm.is_finite() && *bpm > 0.0 {
                    next.master_bpm = *bpm;
                }
                // Otherwise: no-op. The reducer is pure and the
                // SharedClock side ignores bad values too.
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
            EventKind::SetMasterLimiterEnabled { enabled } => {
                next.master_limiter_enabled = *enabled;
            }
            EventKind::SetMasterLimiterThreshold { threshold_db } => {
                next.master_limiter_threshold_db =
                    crate::audio::clamp_master_limiter_threshold_db(*threshold_db);
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
                downbeats_ms: vec![],
                hot_cues: [None; 8],
            },
        ));
        assert_eq!(s.deck_a.bpm, 128.0);
        assert_eq!(s.deck_a.beat_grid_anchor_ms, 220);
        assert!((s.deck_a.beat_period_ms - (60_000.0 / 128.0)).abs() < 0.001);
        assert!(s.deck_a.downbeats.is_empty());
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
                downbeats_ms: vec![],
                hot_cues: [None; 8],
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
                downbeats_ms: vec![],
                hot_cues: [None; 8],
            },
        ));
        let s = s.apply(&ev(2, EventKind::DeckPlay { deck: DeckId::A }));
        assert!(s.deck_a.loaded.is_some());
        let s = s.apply(&ev(3, EventKind::DeckUnload { deck: DeckId::A }));
        assert!(s.deck_a.loaded.is_none());
        assert!(!s.deck_a.playing);
        assert!(s.deck_a.downbeats.is_empty());
    }

    #[test]
    fn deck_load_populates_downbeats() {
        let s = EngineState::default();
        // 4-bar grid at 120 BPM: bar = 4 × 500ms = 2000ms.
        let downbeats: Vec<u32> = (0..10).map(|i| i * 2000).collect();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::B,
                track: TrackRef {
                    id: "tdb".into(),
                    path: "/tracks/db.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: downbeats.clone(),
                hot_cues: [None; 8],
            },
        ));
        assert_eq!(s.deck_b.downbeats.len(), downbeats.len());
        assert_eq!(s.deck_b.downbeats[0], 0);
        assert_eq!(s.deck_b.downbeats[9], 18_000);
        // Per-track grid only — deck A must remain untouched.
        assert!(s.deck_a.downbeats.is_empty());
    }

    #[test]
    fn deck_load_truncates_downbeats_at_inline_capacity() {
        let s = EngineState::default();
        // Synthesize 200 downbeats — well above the inline cap of 64.
        let downbeats: Vec<u32> = (0..200u32).map(|i| i * 2000).collect();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "huge".into(),
                    path: "/tracks/huge.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: downbeats,
                hot_cues: [None; 8],
            },
        ));
        assert_eq!(s.deck_a.downbeats.len(), DOWNBEATS_INLINE_CAPACITY);
        // First 64 entries are preserved (FIFO truncation per the docstring).
        assert_eq!(s.deck_a.downbeats[0], 0);
        assert_eq!(
            s.deck_a.downbeats[DOWNBEATS_INLINE_CAPACITY - 1],
            ((DOWNBEATS_INLINE_CAPACITY - 1) as u32) * 2000,
        );
    }

    #[test]
    fn deck_load_replacing_track_resets_downbeats() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t1".into(),
                    path: "/a.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![0, 2000, 4000, 6000],
                hot_cues: [None; 8],
            },
        ));
        assert_eq!(s.deck_a.downbeats.len(), 4);
        // Replacing track on same deck must overwrite the grid wholesale, not
        // append. Otherwise stale downbeats from the previous track could
        // confuse the proposer's `next_downbeat_after` math.
        let s = s.apply(&ev(
            2,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t2".into(),
                    path: "/b.mp3".into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 100,
                downbeats_ms: vec![100, 1975, 3850],
                hot_cues: [None; 8],
            },
        ));
        assert_eq!(s.deck_a.downbeats.len(), 3);
        assert_eq!(s.deck_a.downbeats[0], 100);
    }

    #[test]
    fn deck_load_populates_hot_cues_from_payload() {
        // Hot-cue persistence PR: DeckLoad now carries an 8-slot
        // hot-cue array (library → engine). The reducer copies it
        // verbatim onto the deck so a track always loads with the
        // cues it was last saved with.
        let s = EngineState::default();
        let cues = [
            Some(0_u64),
            Some(1_500),
            None,
            Some(8_000),
            None,
            None,
            Some(60_000),
            None,
        ];
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "with-cues".into(),
                    path: "/p.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: cues,
            },
        ));
        assert_eq!(s.deck_a.hot_cues, cues);
        // Per-deck only — deck B must be untouched.
        assert!(s.deck_b.hot_cues.iter().all(Option::is_none));
    }

    #[test]
    fn deck_load_default_hot_cues_is_all_none() {
        // Wire-compat: an old DeckLoad payload (pre hot-cue
        // persistence) deserializes with `hot_cues` defaulting to
        // all-None via serde. Construct one explicitly here to mirror
        // that semantic (the default function under test).
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::B,
                track: TrackRef {
                    id: "no-cues".into(),
                    path: "/p.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: super::default_hot_cues(),
            },
        ));
        assert!(s.deck_b.hot_cues.iter().all(Option::is_none));
    }

    #[test]
    fn deck_load_replaces_existing_hot_cues_wholesale() {
        // Loading a *new* track on a deck must overwrite any prior
        // hot-cues — otherwise stale cues from the previous track
        // would phantom-trigger when the user hits a pad on the new
        // track. Mirrors the same contract as `downbeats` reset.
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t1".into(),
                    path: "/a.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [
                    Some(1_000),
                    Some(2_000),
                    Some(3_000),
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
            },
        ));
        assert_eq!(s.deck_a.hot_cues[0], Some(1_000));
        // Load a different track with a different cue layout — old
        // values must NOT persist.
        let s = s.apply(&ev(
            2,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t2".into(),
                    path: "/b.mp3".into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None, None, None, None, None, None, None, Some(99_000)],
            },
        ));
        assert_eq!(s.deck_a.hot_cues[0], None);
        assert_eq!(s.deck_a.hot_cues[7], Some(99_000));
    }

    #[test]
    fn deck_load_hot_cues_serde_roundtrip_with_default() {
        // Serde-default behaviour: an *omitted* `hot_cues` field in
        // a JSON payload still deserializes (default = all-None).
        // This catches accidental removal of `#[serde(default = ...)]`.
        let json = r#"{
            "DeckLoad": {
                "deck": "A",
                "track": { "id": "t1", "path": "/p.mp3" },
                "bpm": 120.0,
                "beat_grid_anchor_ms": 0
            }
        }"#;
        let kind: EventKind = serde_json::from_str(json).expect("deserialize");
        match kind {
            EventKind::DeckLoad { hot_cues, .. } => {
                assert!(hot_cues.iter().all(Option::is_none));
            }
            other => panic!("expected DeckLoad, got {other:?}"),
        }
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

    #[test]
    fn deck_default_tempo_ratio_is_one() {
        // Pitch/tempo PR: tempo_ratio must default to 1.0, not the
        // f32::default() of 0.0. Catches a regression of the manual
        // `Default` impl reverting to the auto-derive.
        let d: Deck = Default::default();
        assert!((d.tempo_ratio - 1.0).abs() < f32::EPSILON);
        assert!(d.pitch_semitones.abs() < f32::EPSILON);
    }

    #[test]
    fn tempo_bend_sets_ratio_clamped() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 1.25,
            },
        ));
        assert!((s.deck_a.tempo_ratio - 1.25).abs() < 1e-6);
        // Over-range clamps to max.
        let s = s.apply(&ev(
            2,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 10.0,
            },
        ));
        assert!((s.deck_a.tempo_ratio - crate::audio::MAX_TEMPO_RATIO).abs() < 1e-6);
        // Under-range clamps to min.
        let s = s.apply(&ev(
            3,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 0.0,
            },
        ));
        assert!((s.deck_a.tempo_ratio - crate::audio::MIN_TEMPO_RATIO).abs() < 1e-6);
        // NaN safely falls back to 1.0.
        let s = s.apply(&ev(
            4,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: f32::NAN,
            },
        ));
        assert!((s.deck_a.tempo_ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pitch_tempo_reset_returns_both_to_defaults() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::PitchBend {
                deck: DeckId::A,
                semitones: 5.0,
            },
        ));
        let s = s.apply(&ev(
            2,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 1.5,
            },
        ));
        assert!((s.deck_a.pitch_semitones - 5.0).abs() < 1e-6);
        assert!((s.deck_a.tempo_ratio - 1.5).abs() < 1e-6);
        let s = s.apply(&ev(3, EventKind::PitchTempoReset { deck: DeckId::A }));
        assert!(s.deck_a.pitch_semitones.abs() < f32::EPSILON);
        assert!((s.deck_a.tempo_ratio - 1.0).abs() < f32::EPSILON);
        // Other deck untouched.
        assert!((s.deck_b.tempo_ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pitch_bend_does_not_modify_tempo_ratio() {
        // Pitch/tempo independence at the state-log level: bending pitch
        // never touches tempo_ratio, even when its previous value was
        // non-default.
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 0.8,
            },
        ));
        assert!((s.deck_a.tempo_ratio - 0.8).abs() < 1e-6);
        let s = s.apply(&ev(
            2,
            EventKind::PitchBend {
                deck: DeckId::A,
                semitones: 7.0,
            },
        ));
        assert!((s.deck_a.pitch_semitones - 7.0).abs() < 1e-6);
        assert!(
            (s.deck_a.tempo_ratio - 0.8).abs() < 1e-6,
            "PitchBend must not touch tempo_ratio"
        );
    }

    #[test]
    fn master_limiter_enabled_by_default() {
        // Safety-first default — limiter ON the moment the engine starts
        // so a hot session can't clip the master bus or the recording.
        let s = EngineState::default();
        assert!(s.master_limiter_enabled);
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn set_master_limiter_enabled_toggles() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::SetMasterLimiterEnabled { enabled: false },
        ));
        assert!(!s.master_limiter_enabled);
        let s = s.apply(&ev(2, EventKind::SetMasterLimiterEnabled { enabled: true }));
        assert!(s.master_limiter_enabled);
    }

    #[test]
    fn set_master_limiter_threshold_clamps_to_window() {
        let s = EngineState::default();
        // Over-max → clamp to 0 dB.
        let s = s.apply(&ev(
            1,
            EventKind::SetMasterLimiterThreshold { threshold_db: 12.0 },
        ));
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_MAX_THRESHOLD_DB).abs()
                < 1e-6
        );
        // Under-min → clamp to -24 dB.
        let s = s.apply(&ev(
            2,
            EventKind::SetMasterLimiterThreshold {
                threshold_db: -100.0,
            },
        ));
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_MIN_THRESHOLD_DB).abs()
                < 1e-6
        );
        // NaN → default.
        let s = s.apply(&ev(
            3,
            EventKind::SetMasterLimiterThreshold {
                threshold_db: f32::NAN,
            },
        ));
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn engine_state_serde_roundtrip_preserves_limiter_fields() {
        // Wire-compat: an older snapshot (pre-limiter) deserializes
        // back to defaults via the `#[serde(default)]` attributes —
        // catches accidental removal of those serde defaults.
        let json = r#"{
            "deck_a": {
                "loaded": null, "playing": false, "position_ms": 0,
                "pitch_semitones": 0.0, "tempo_ratio": 1.0,
                "eq_low_db": 0.0, "eq_mid_db": 0.0, "eq_high_db": 0.0,
                "loop_in_ms": null, "loop_out_ms": null, "loop_active": false,
                "hot_cues": [null,null,null,null,null,null,null,null],
                "copilot_engaged": false, "bpm": 0.0, "beat_grid_anchor_ms": 0,
                "beat_period_ms": 0.0, "phase_offset_ms": 0,
                "effects": [{"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false},
                            {"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false},
                            {"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false}],
                "handoff_until_frame": 0
            },
            "deck_b": {
                "loaded": null, "playing": false, "position_ms": 0,
                "pitch_semitones": 0.0, "tempo_ratio": 1.0,
                "eq_low_db": 0.0, "eq_mid_db": 0.0, "eq_high_db": 0.0,
                "loop_in_ms": null, "loop_out_ms": null, "loop_active": false,
                "hot_cues": [null,null,null,null,null,null,null,null],
                "copilot_engaged": false, "bpm": 0.0, "beat_grid_anchor_ms": 0,
                "beat_period_ms": 0.0, "phase_offset_ms": 0,
                "effects": [{"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false},
                            {"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false},
                            {"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false}],
                "handoff_until_frame": 0
            },
            "crossfader": 0.5,
            "master_volume_db": 0.0,
            "session_active": false
        }"#;
        let s: EngineState = serde_json::from_str(json).expect("old snapshot must deserialize");
        // Missing limiter fields fall back to the serde defaults.
        assert!(s.master_limiter_enabled);
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB)
                .abs()
                < 1e-6
        );
    }
}
