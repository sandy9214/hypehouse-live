//! Control-thread → audio-thread translator.
//!
//! [`event_to_commands`] is a **pure function** that diffs a previous and
//! next [`EngineState`] in light of an applied [`Event`] and emits the
//! corresponding [`AudioCommand`]s. The control thread is allowed to
//! allocate, but we keep the common case heap-free by returning a
//! `SmallVec<[AudioCommand; 4]>` (most events emit 0..1 commands; 4 is a
//! comfortable upper bound for the few that emit several).
//!
//! Sequencing rules (ADR-004 §"State-log → command translation"):
//!
//! 1. Diff `prev_state` vs `next_state`.
//! 2. Emit `AudioCommand`s with appropriate `at_frame` (mostly "now" =
//!    next buffer boundary).
//! 3. The control thread pushes the result onto the SPSC ring.
//!
//! Special cases:
//!
//! * **DeckLoad** — the event itself signals "start decoding". The
//!   translator calls `DecodeService::open()`, which spawns a
//!   per-track decoder thread off the control plane and returns a
//!   `DecodeHandle`. The translator emits an
//!   `AudioCommandKind::DeckLoad { deck, handle }` for the audio
//!   thread to start pulling streaming frames via
//!   `DecodeService::read`. See `decode.rs` for the streaming pipeline.
//! * **TakeOver** — emits two commands (no immediate audio side effect
//!   on the deck's current envelope): `ArmHandoff` (so the audio thread
//!   knows the 1-bar window) + `CancelAfter` (so queued AI commands
//!   past the envelope are dropped — ADR-005).

use smallvec::SmallVec;

use crate::audio::{
    command::{AudioCommand, AudioCommandKind},
    decode::DecodeService,
};
use crate::state::{EngineState, EqBand, Event, EventKind};

/// SmallVec inline capacity — the common case is 0..1 commands per
/// event; we size for 4 so the rare multi-emit cases (`TakeOver`,
/// `DeckLoad`) don't escape to the heap.
pub type AudioCmdBatch = SmallVec<[AudioCommand; 4]>;

/// Beats per bar in 4/4 time. ADR-005's "1-bar handoff envelope" uses
/// this constant.
pub const BAR_BEATS: u32 = 4;

/// Default smooth-ramp duration for continuous parameters
/// (crossfader, EQ, pitch) to mask zipper noise. 5ms at 48kHz = 240
/// frames; per ADR-004 §"AudioCommand shape (v0)" comment "smooth-ramp,
/// no zipper noise".
pub const DEFAULT_RAMP_MS: f32 = 5.0;

#[inline]
fn ramp_frames(sample_rate: u32) -> u32 {
    // 5 ms × sample_rate / 1000. f32 safe at 48–192 kHz.
    ((DEFAULT_RAMP_MS / 1000.0) * sample_rate as f32) as u32
}

/// Diff `prev` vs `next` in light of `ev` and emit the commands the
/// audio thread needs to execute the state change.
///
/// Pure. Does not push to the ring — caller does that.
///
/// `decode` is the (stub for now) decode service the translator asks for
/// buffers when an event would require new pre-decoded audio.
pub fn event_to_commands(
    prev: &EngineState,
    next: &EngineState,
    ev: &Event,
    now_frame: u64,
    sample_rate: u32,
    decode: &dyn DecodeService,
) -> AudioCmdBatch {
    let mut out: AudioCmdBatch = SmallVec::new();
    let ramp = ramp_frames(sample_rate);

    match &ev.kind {
        EventKind::DeckPlay { deck } => {
            // Only emit if the deck actually transitioned.
            if !prev.deck_ref(*deck).playing && next.deck_ref(*deck).playing {
                out.push(AudioCommand {
                    at_frame: now_frame,
                    kind: AudioCommandKind::DeckPlay { deck: *deck },
                });
            }
        }
        EventKind::DeckPause { deck } => {
            if prev.deck_ref(*deck).playing && !next.deck_ref(*deck).playing {
                out.push(AudioCommand {
                    at_frame: now_frame,
                    kind: AudioCommandKind::DeckPause { deck: *deck },
                });
            }
        }
        EventKind::DeckCue { deck, position_ms } => {
            // ms → frames at the audio thread's sample rate.
            let frame = ms_to_frames(*position_ms, sample_rate);
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::DeckSeek { deck: *deck, frame },
            });
        }
        EventKind::Crossfader { .. } => {
            // Always emit with a ramp so we never zipper.
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::Crossfader {
                    target: next.crossfader,
                    ramp_frames: ramp,
                },
            });
        }
        EventKind::EqAdjust { deck, band, .. } => {
            let target = match band {
                EqBand::Low => next.deck_ref(*deck).eq_low_db,
                EqBand::Mid => next.deck_ref(*deck).eq_mid_db,
                EqBand::High => next.deck_ref(*deck).eq_high_db,
            };
            let kind = match band {
                EqBand::Low => AudioCommandKind::EqLow {
                    deck: *deck,
                    target_db: target,
                    ramp_frames: ramp,
                },
                EqBand::Mid => AudioCommandKind::EqMid {
                    deck: *deck,
                    target_db: target,
                    ramp_frames: ramp,
                },
                EqBand::High => AudioCommandKind::EqHigh {
                    deck: *deck,
                    target_db: target,
                    ramp_frames: ramp,
                },
            };
            out.push(AudioCommand {
                at_frame: now_frame,
                kind,
            });
        }
        EventKind::PitchBend { deck, .. } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::Pitch {
                    deck: *deck,
                    semitones: next.deck_ref(*deck).pitch_semitones,
                    ramp_frames: ramp,
                },
            });
        }
        EventKind::TempoBend { deck, .. } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::Tempo {
                    deck: *deck,
                    ratio: next.deck_ref(*deck).tempo_ratio,
                    ramp_frames: ramp,
                },
            });
        }
        EventKind::PitchTempoReset { deck } => {
            // Single audio-command — the mixer collapses both knobs +
            // resets the rubato cascade in one shot. Keeps the
            // SmallVec to a length of 1 (cheap inline path).
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::PitchTempoReset { deck: *deck },
            });
        }
        EventKind::LoopOut { deck } => {
            let d = next.deck_ref(*deck);
            if d.loop_active {
                if let (Some(in_ms), Some(out_ms)) = (d.loop_in_ms, d.loop_out_ms) {
                    out.push(AudioCommand {
                        at_frame: now_frame,
                        kind: AudioCommandKind::LoopArm {
                            deck: *deck,
                            in_frame: ms_to_frames(in_ms, sample_rate),
                            out_frame: ms_to_frames(out_ms, sample_rate),
                        },
                    });
                }
            }
        }
        EventKind::LoopExit { deck } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::LoopDisarm { deck: *deck },
            });
        }
        EventKind::DeckLoad { deck, track, .. } => {
            // Ask the streaming decode service to open the track. The
            // service spawns a decoder thread off the control plane;
            // the returned handle is what the audio thread reads
            // from in subsequent render calls. `open` errors land
            // here as `tracing::warn!` + no command emitted — the UI
            // sees a no-op load (issue #TBD: surface load errors via
            // an engine event).
            match decode.open(track, sample_rate) {
                Ok(handle) => {
                    out.push(AudioCommand {
                        at_frame: now_frame,
                        kind: AudioCommandKind::DeckLoad {
                            deck: *deck,
                            handle,
                        },
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        target: "decode",
                        track_id = %track.id,
                        path = %track.path,
                        error = ?e,
                        "DecodeService::open failed — deck will not load",
                    );
                }
            }
        }
        EventKind::DeckUnload { deck } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::DeckUnload { deck: *deck },
            });
        }
        EventKind::HotCueTrigger { deck, .. } => {
            // Seek to whatever position the reducer landed on.
            let frame = ms_to_frames(next.deck_ref(*deck).position_ms, sample_rate);
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::DeckSeek { deck: *deck, frame },
            });
        }
        EventKind::TakeOver {
            deck,
            handoff_until_frame,
        } => {
            // ADR-005 — no immediate audio command on the deck's
            // running envelope; just arm the 1-bar handoff window plus
            // cancel any AI commands queued past it. The audio
            // thread's existing AI automation continues until
            // `handoff_until_frame`; user inputs cross-fade in
            // automatically via the ring's natural per-buffer drain.
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::ArmHandoff {
                    deck: *deck,
                    until_frame: *handoff_until_frame,
                },
            });
            let after_frames = handoff_until_frame.saturating_sub(now_frame) as u32;
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::CancelAfter {
                    deck: *deck,
                    after_frames,
                },
            });
        }
        // ADR-006 — effects: emit POD `AudioCommandKind::Effect*` per
        // event so the audio thread can mutate its `FxBank` state.
        // String params are resolved to numeric `param_id` here on
        // the control thread; the audio side never sees a `String`.
        EventKind::EffectAssign {
            deck,
            slot,
            effect_id,
        } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::EffectAssign {
                    deck: *deck,
                    slot: *slot,
                    effect_id: *effect_id,
                },
            });
        }
        EventKind::EffectClear { deck, slot } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::EffectClear {
                    deck: *deck,
                    slot: *slot,
                },
            });
        }
        EventKind::EffectParam {
            deck,
            slot,
            param,
            value,
        } => {
            // Resolve param name → numeric id by asking the registry
            // for the slot's current effect. Drop the command silently
            // if the slot is empty or the name is unknown.
            let effect_id = next
                .deck_ref(*deck)
                .effects
                .get(*slot as usize)
                .map(|s| s.effect_id)
                .unwrap_or(0);
            if let Some(param_id) = crate::audio::effects::resolve_param(effect_id, param) {
                out.push(AudioCommand {
                    at_frame: now_frame,
                    kind: AudioCommandKind::EffectParam {
                        deck: *deck,
                        slot: *slot,
                        param_id,
                        value: *value,
                    },
                });
            }
        }
        EventKind::EffectWetDry { deck, slot, value } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::EffectWetDry {
                    deck: *deck,
                    slot: *slot,
                    value: *value,
                },
            });
        }
        EventKind::EffectEnable {
            deck,
            slot,
            enabled,
        } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::EffectEnable {
                    deck: *deck,
                    slot: *slot,
                    enabled: *enabled,
                },
            });
        }
        // Non-audio-relevant events — pure state, no audio command needed.
        // (`SetMasterBpm` updates the SharedClock side-channel separately;
        // see ADR-007 §v0.1 — the audio thread doesn't consume it.)
        EventKind::HotCueSet { .. }
        | EventKind::LoopIn { .. }
        | EventKind::PhaseNudge { .. }
        | EventKind::CopilotEngage { .. }
        | EventKind::CopilotDisengage { .. }
        | EventKind::SetMasterBpm { .. }
        | EventKind::SessionStart
        | EventKind::SessionEnd => {}
    }

    let _ = prev; // keep the diff-style signature even when unused in some arms
    out
}

/// Convert track-relative milliseconds to absolute sample frames.
#[inline]
fn ms_to_frames(ms: u64, sample_rate: u32) -> u64 {
    // ms × sr / 1000. Use u128 to avoid overflow on very long tracks.
    ((ms as u128) * (sample_rate as u128) / 1000) as u64
}

/// Tiny extension on `EngineState` so the translator can index a deck
/// by `DeckId` without cloning. Read-only — does NOT change state.rs.
trait DeckRef {
    fn deck_ref(&self, id: crate::state::DeckId) -> &crate::state::Deck;
}

impl DeckRef for EngineState {
    fn deck_ref(&self, id: crate::state::DeckId) -> &crate::state::Deck {
        match id {
            crate::state::DeckId::A => &self.deck_a,
            crate::state::DeckId::B => &self.deck_b,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decode::StubDecodeService;
    use crate::state::{DeckId, EventSource, TrackRef};

    const SR: u32 = 48_000;

    fn ev(id: u64, kind: EventKind) -> Event {
        Event {
            id,
            ts_micros: 0,
            source: EventSource::Ui,
            kind,
        }
    }

    fn translate(prev: &EngineState, next: &EngineState, e: &Event, now: u64) -> AudioCmdBatch {
        let decode = StubDecodeService::new();
        event_to_commands(prev, next, e, now, SR, &decode)
    }

    #[test]
    fn deck_play_emits_one_deck_play_command() {
        let prev = EngineState::default();
        let e = ev(1, EventKind::DeckPlay { deck: DeckId::A });
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1, "DeckPlay should emit exactly one command");
        match cmds[0].kind {
            AudioCommandKind::DeckPlay { deck } => assert_eq!(deck, DeckId::A),
            other => panic!("expected DeckPlay, got {other:?}"),
        }
    }

    #[test]
    fn deck_play_when_already_playing_is_idempotent() {
        // Already-playing state → applying DeckPlay again is a no-op.
        let prev = EngineState::default().apply(&ev(1, EventKind::DeckPlay { deck: DeckId::A }));
        let e = ev(2, EventKind::DeckPlay { deck: DeckId::A });
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert!(cmds.is_empty(), "no-op DeckPlay shouldn't re-trigger audio");
    }

    #[test]
    fn crossfader_includes_ramp_frames_above_zero() {
        let prev = EngineState::default();
        let e = ev(1, EventKind::Crossfader { value: 0.75 });
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 1024);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::Crossfader {
                target,
                ramp_frames,
            } => {
                assert!((target - 0.75).abs() < 1e-6);
                assert!(
                    ramp_frames > 0,
                    "crossfader ramp_frames must be > 0 to avoid zipper noise"
                );
                // 5ms at 48kHz = 240 frames.
                assert_eq!(ramp_frames, 240);
            }
            other => panic!("expected Crossfader, got {other:?}"),
        }
    }

    #[test]
    fn takeover_arms_handoff_and_cancels_pending() {
        // Set up: deck A is co-pilot-engaged, then user takes over.
        let s0 = EngineState::default();
        let s1 = s0.apply(&ev(1, EventKind::CopilotEngage { deck: DeckId::A }));
        let handoff = 96_000_u64; // ~2s at 48kHz = ~1 bar at 120 BPM
        let e = ev(
            2,
            EventKind::TakeOver {
                deck: DeckId::A,
                handoff_until_frame: handoff,
            },
        );
        let s2 = s1.apply(&e);
        let cmds = translate(&s1, &s2, &e, 0);
        assert_eq!(
            cmds.len(),
            2,
            "TakeOver should emit ArmHandoff + CancelAfter"
        );
        let arm = cmds
            .iter()
            .find(|c| matches!(c.kind, AudioCommandKind::ArmHandoff { .. }));
        let cancel = cmds
            .iter()
            .find(|c| matches!(c.kind, AudioCommandKind::CancelAfter { .. }));
        let arm = arm.expect("ArmHandoff missing");
        let cancel = cancel.expect("CancelAfter missing");
        match arm.kind {
            AudioCommandKind::ArmHandoff { deck, until_frame } => {
                assert_eq!(deck, DeckId::A);
                assert_eq!(until_frame, handoff);
            }
            _ => unreachable!(),
        }
        match cancel.kind {
            AudioCommandKind::CancelAfter { deck, after_frames } => {
                assert_eq!(deck, DeckId::A);
                assert_eq!(after_frames as u64, handoff);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn eq_adjust_emits_band_specific_command() {
        let prev = EngineState::default();
        let e_low = ev(
            1,
            EventKind::EqAdjust {
                deck: DeckId::A,
                band: EqBand::Low,
                value_db: -6.0,
            },
        );
        let next = prev.apply(&e_low);
        let cmds = translate(&prev, &next, &e_low, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::EqLow {
                deck,
                target_db,
                ramp_frames,
            } => {
                assert_eq!(deck, DeckId::A);
                assert!((target_db - (-6.0)).abs() < 1e-6);
                assert!(ramp_frames > 0);
            }
            other => panic!("expected EqLow, got {other:?}"),
        }

        // Mid band.
        let e_mid = ev(
            2,
            EventKind::EqAdjust {
                deck: DeckId::B,
                band: EqBand::Mid,
                value_db: 3.0,
            },
        );
        let next = next.apply(&e_mid);
        let cmds = translate(&prev, &next, &e_mid, 0);
        assert!(matches!(cmds[0].kind, AudioCommandKind::EqMid { .. }));

        // High band.
        let e_high = ev(
            3,
            EventKind::EqAdjust {
                deck: DeckId::B,
                band: EqBand::High,
                value_db: -2.0,
            },
        );
        let next = next.apply(&e_high);
        let cmds = translate(&prev, &next, &e_high, 0);
        assert!(matches!(cmds[0].kind, AudioCommandKind::EqHigh { .. }));
    }

    #[test]
    fn deck_load_emits_load_buffer_for_correct_deck() {
        let prev = EngineState::default();
        let e = ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::B,
                track: TrackRef {
                    id: "t1".into(),
                    path: "/tracks/x.mp3".into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
            },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::DeckLoad { deck, handle } => {
                assert_eq!(deck, DeckId::B);
                assert!(
                    handle.is_some(),
                    "stub decode service should hand out valid handles"
                );
            }
            other => panic!("expected DeckLoad, got {other:?}"),
        }
    }

    #[test]
    fn copilot_engage_emits_no_audio_command() {
        let prev = EngineState::default();
        let e = ev(1, EventKind::CopilotEngage { deck: DeckId::A });
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert!(
            cmds.is_empty(),
            "control-plane-only events emit no audio cmds"
        );
    }

    #[test]
    fn ms_to_frames_roundtrip() {
        assert_eq!(ms_to_frames(1000, 48_000), 48_000);
        assert_eq!(ms_to_frames(0, 48_000), 0);
        assert_eq!(ms_to_frames(500, 96_000), 48_000);
    }

    #[test]
    fn effect_assign_emits_audio_command() {
        // ADR-006 — EffectAssign event must translate into an
        // EffectAssign audio command so the mixer's FxBank picks it
        // up.
        let prev = EngineState::default();
        let e = ev(
            1,
            EventKind::EffectAssign {
                deck: DeckId::A,
                slot: 0,
                effect_id: 2, // Echo
            },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::EffectAssign {
                deck,
                slot,
                effect_id,
            } => {
                assert_eq!(deck, DeckId::A);
                assert_eq!(slot, 0);
                assert_eq!(effect_id, 2);
            }
            other => panic!("expected EffectAssign, got {other:?}"),
        }
    }

    #[test]
    fn effect_param_resolves_param_name_to_numeric_id() {
        // After assigning Filter to slot 0, an EffectParam event with
        // `param="cutoff_hz"` should resolve to param_id=0 (the
        // descriptor index of cutoff_hz).
        let s0 = EngineState::default();
        let assign = ev(
            1,
            EventKind::EffectAssign {
                deck: DeckId::A,
                slot: 0,
                effect_id: 1, // Filter
            },
        );
        let s1 = s0.apply(&assign);
        let set = ev(
            2,
            EventKind::EffectParam {
                deck: DeckId::A,
                slot: 0,
                param: "cutoff_hz".to_string(),
                value: 800.0,
            },
        );
        let s2 = s1.apply(&set);
        let cmds = translate(&s1, &s2, &set, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::EffectParam {
                deck,
                slot,
                param_id,
                value,
            } => {
                assert_eq!(deck, DeckId::A);
                assert_eq!(slot, 0);
                assert_eq!(param_id, 0);
                assert!((value - 800.0).abs() < 1e-6);
            }
            other => panic!("expected EffectParam, got {other:?}"),
        }
    }

    #[test]
    fn effect_param_unknown_name_is_dropped() {
        // Unknown param name → no command emitted (silent drop; the
        // reducer still records the state change for audit, but the
        // audio side has no slot to receive it).
        let s0 = EngineState::default();
        let assign = ev(
            1,
            EventKind::EffectAssign {
                deck: DeckId::A,
                slot: 0,
                effect_id: 1,
            },
        );
        let s1 = s0.apply(&assign);
        let bad = ev(
            2,
            EventKind::EffectParam {
                deck: DeckId::A,
                slot: 0,
                param: "not_a_param".to_string(),
                value: 1.0,
            },
        );
        let s2 = s1.apply(&bad);
        let cmds = translate(&s1, &s2, &bad, 0);
        assert!(
            cmds.is_empty(),
            "unknown effect param should drop without emitting a command"
        );
    }

    #[test]
    fn effect_wet_dry_and_enable_translate() {
        let s0 = EngineState::default();
        let assign = ev(
            1,
            EventKind::EffectAssign {
                deck: DeckId::B,
                slot: 1,
                effect_id: 3,
            },
        );
        let s1 = s0.apply(&assign);
        let wd = ev(
            2,
            EventKind::EffectWetDry {
                deck: DeckId::B,
                slot: 1,
                value: 0.8,
            },
        );
        let s2 = s1.apply(&wd);
        let cmds = translate(&s1, &s2, &wd, 0);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0].kind,
            AudioCommandKind::EffectWetDry { .. }
        ));
        let en = ev(
            3,
            EventKind::EffectEnable {
                deck: DeckId::B,
                slot: 1,
                enabled: false,
            },
        );
        let s3 = s2.apply(&en);
        let cmds = translate(&s2, &s3, &en, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::EffectEnable {
                deck,
                slot,
                enabled,
            } => {
                assert_eq!(deck, DeckId::B);
                assert_eq!(slot, 1);
                assert!(!enabled);
            }
            other => panic!("expected EffectEnable, got {other:?}"),
        }
    }

    #[test]
    fn tempo_bend_emits_tempo_command_with_clamped_value() {
        // Pitch/tempo-independent PR — TempoBend translates 1-1 to an
        // AudioCommandKind::Tempo carrying the deck's `tempo_ratio`
        // *after* the reducer's clamp.
        let prev = EngineState::default();
        let e = ev(
            1,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 1.5,
            },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::Tempo {
                deck,
                ratio,
                ramp_frames,
            } => {
                assert_eq!(deck, DeckId::A);
                assert!((ratio - 1.5).abs() < 1e-6);
                assert!(ramp_frames > 0);
            }
            other => panic!("expected Tempo command, got {other:?}"),
        }
    }

    #[test]
    fn tempo_bend_out_of_range_emits_clamped_value() {
        // ratio 10.0 reducer-clamps to MAX_TEMPO_RATIO; the translator
        // must forward the clamped value, not the raw input.
        let prev = EngineState::default();
        let e = ev(
            1,
            EventKind::TempoBend {
                deck: DeckId::B,
                ratio: 10.0,
            },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        match cmds[0].kind {
            AudioCommandKind::Tempo { ratio, .. } => {
                assert!((ratio - crate::audio::MAX_TEMPO_RATIO).abs() < 1e-6);
            }
            other => panic!("expected Tempo, got {other:?}"),
        }
    }

    #[test]
    fn pitch_tempo_reset_emits_single_audio_command() {
        // Convenience event collapses to one command — the mixer's
        // PitchTempoReset path resets both knobs + the rubato state.
        let prev = EngineState::default();
        let e = ev(1, EventKind::PitchTempoReset { deck: DeckId::A });
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::PitchTempoReset { deck } => assert_eq!(deck, DeckId::A),
            other => panic!("expected PitchTempoReset, got {other:?}"),
        }
    }

    #[test]
    fn deck_load_emits_valid_handle_each_call() {
        // Streaming decode model: every `open` spawns a fresh
        // decoder thread + returns a fresh handle. This is a
        // deliberate change from the v0.1 pre-decoded-buffer cache
        // (which content-addressed by track id) — re-loading the
        // same track yields a new handle so seek state, etc., is
        // fresh.
        let prev = EngineState::default();
        let make = |id: u64| {
            ev(
                id,
                EventKind::DeckLoad {
                    deck: DeckId::A,
                    track: TrackRef {
                        id: "song-7".into(),
                        path: "/p".into(),
                    },
                    bpm: 120.0,
                    beat_grid_anchor_ms: 0,
                    downbeats_ms: vec![],
                },
            )
        };
        let e1 = make(1);
        let e2 = make(2);
        let next1 = prev.apply(&e1);
        let next2 = next1.apply(&e2);
        let decode = StubDecodeService::new();
        let cmds1 = event_to_commands(&prev, &next1, &e1, 0, SR, &decode);
        let cmds2 = event_to_commands(&next1, &next2, &e2, 0, SR, &decode);
        let id1 = match cmds1[0].kind {
            AudioCommandKind::DeckLoad { handle, .. } => handle,
            _ => unreachable!(),
        };
        let id2 = match cmds2[0].kind {
            AudioCommandKind::DeckLoad { handle, .. } => handle,
            _ => unreachable!(),
        };
        assert!(id1.is_some());
        assert!(id2.is_some());
    }
}
