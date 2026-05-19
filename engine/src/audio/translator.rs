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
    decode::{DecodeError, DecodeService},
};
use crate::state::{DeckId, EngineState, EqBand, Event, EventKind};

/// SmallVec inline capacity — the common case is 0..1 commands per
/// event; we size for 4 so the rare multi-emit cases (`TakeOver`,
/// `DeckLoad`) don't escape to the heap.
pub type AudioCmdBatch = SmallVec<[AudioCommand; 4]>;

/// Out-of-band failure surfaced by [`event_to_commands_with_errors`].
///
/// The translator's pure-fn contract means it cannot publish to the
/// bridge's broadcast channel itself; instead it returns a list of
/// failures the caller (the control loop) drains and forwards to
/// `EngineHandle::publish_decode_error`. Today only `DeckLoad` populates
/// this list; the variant carries the deck the load was targeting plus
/// the underlying [`DecodeError`] for diagnostic context.
///
/// The control loop maps `kind` into a coarse `category` string for the
/// wire (`file_not_found`, `format_unsupported`, `decoder_error`,
/// `resource_exhausted`, `unknown_inline_source`, `decoder_thread_spawn`)
/// via [`DecodeFailure::category`].
#[derive(Debug)]
pub struct DecodeFailure {
    /// Deck the failed load was targeting.
    pub deck: DeckId,
    /// Track-id from the originating `DeckLoad` event.
    pub track_id: String,
    /// Source `DecodeError`. The control loop stringifies this for the
    /// `error` wire field; tests inspect it directly.
    pub error: DecodeError,
}

impl DecodeFailure {
    /// Coarse failure class for the UI toast. Stable string namespace
    /// so the frontend can branch on it for icons / copy without
    /// pattern-matching the underlying error message.
    pub fn category(&self) -> &'static str {
        match &self.error {
            DecodeError::Io { .. } => "file_not_found",
            DecodeError::Probe(_) => "format_unsupported",
            DecodeError::NoTrack => "format_unsupported",
            DecodeError::Resampler(_) => "decoder_error",
            DecodeError::NoFreeSlot => "resource_exhausted",
            DecodeError::UnknownInlineSource(_) => "unknown_inline_source",
            DecodeError::Spawn(_) => "decoder_thread_spawn",
        }
    }
}

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
///
/// This is a thin shim over [`event_to_commands_with_errors`] that drops
/// the decode-error sidecar; preserved for callers (benches, older test
/// helpers) that don't care about out-of-band failures.
pub fn event_to_commands(
    prev: &EngineState,
    next: &EngineState,
    ev: &Event,
    now_frame: u64,
    sample_rate: u32,
    decode: &dyn DecodeService,
) -> AudioCmdBatch {
    let (cmds, _errors) =
        event_to_commands_with_errors(prev, next, ev, now_frame, sample_rate, decode);
    cmds
}

/// Same as [`event_to_commands`] but also returns any `DecodeFailure`s
/// observed during translation (today, only `DeckLoad` populates this
/// list — the audio thread itself has no path to fail an `open` after
/// the fact).
///
/// The control loop calls this variant and forwards each failure to
/// `EngineHandle::publish_decode_error` so connected UI clients see a
/// transient toast. State is **not** mutated for failed loads — the
/// deck simply stays empty (preserves the v0.1 "silent no-op load"
/// contract aside from the new notification).
pub fn event_to_commands_with_errors(
    prev: &EngineState,
    next: &EngineState,
    ev: &Event,
    now_frame: u64,
    sample_rate: u32,
    decode: &dyn DecodeService,
) -> (AudioCmdBatch, SmallVec<[DecodeFailure; 1]>) {
    let mut out: AudioCmdBatch = SmallVec::new();
    let mut errors: SmallVec<[DecodeFailure; 1]> = SmallVec::new();
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
        EventKind::SetCrossfaderCurve { .. } => {
            // Read the reducer-finalized curve so the audio thread +
            // state log always agree (mirrors the limiter pattern).
            // Curve switch is metadata only — no smoothing needed; the
            // gain lookup re-evaluates at the next render block, and
            // the per-sample crossfader value is already ramp-smoothed
            // by the `Crossfader` command on the same frame.
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::SetCrossfaderCurve {
                    curve: next.crossfader_curve,
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
        EventKind::SetLoopBars { deck, .. } => {
            // Bar-aware auto-loop. Read the post-reducer loop bounds so
            // the audio thread + state log agree on the exact in/out
            // window (including any beat-grid snap the reducer applied).
            // The reducer no-ops on beat-gridless decks, so a missing
            // `loop_in_ms` / `loop_out_ms` here just means there's
            // nothing to arm on the audio side.
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
        EventKind::DeckLoad {
            deck,
            track,
            track_gain_db,
            ..
        } => {
            // Ask the streaming decode service to open the track. The
            // service spawns a decoder thread off the control plane;
            // the returned handle is what the audio thread reads
            // from in subsequent render calls. `open` errors are
            // logged + pushed onto the `errors` sidecar so the
            // control loop can fan them out as `engine.decode_error`
            // notifications to connected clients. State stays
            // un-mutated — the deck simply doesn't load.
            //
            // `track_gain_db` is forwarded verbatim — the mixer
            // applies the linear conversion + per-sample multiply.
            // Non-finite values are filtered out by the reducer's
            // guard above, but the mixer also defensive-clamps.
            match decode.open(track, sample_rate) {
                Ok(handle) => {
                    out.push(AudioCommand {
                        at_frame: now_frame,
                        kind: AudioCommandKind::DeckLoad {
                            deck: *deck,
                            handle,
                            track_gain_db: *track_gain_db,
                        },
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        target: "decode",
                        track_id = %track.id,
                        path = %track.path,
                        error = ?e,
                        "DecodeService::open failed — surfacing to UI as decode_error",
                    );
                    errors.push(DecodeFailure {
                        deck: *deck,
                        track_id: track.id.clone(),
                        error: e,
                    });
                }
            }
        }
        EventKind::DeckUnload { deck } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::DeckUnload { deck: *deck },
            });
        }
        EventKind::DeckLoadStems {
            deck,
            track,
            stem_paths,
            ..
        } => {
            // Ask the decode service to open all 4 stem WAVs. On
            // partial failure the service rolls back already-opened
            // stems; the deck silently stays empty + a single
            // `DecodeFailure` is published. State stays unmutated
            // (the reducer's stem_mode flip is harmless — the audio
            // thread never receives the StemHandle so the mixer
            // keeps playing whatever was there before).
            match decode.open_stems(track, stem_paths, sample_rate) {
                Ok(stems) => {
                    out.push(AudioCommand {
                        at_frame: now_frame,
                        kind: AudioCommandKind::DeckLoadStems { deck: *deck, stems },
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        target: "decode",
                        track_id = %track.id,
                        error = ?e,
                        "DecodeService::open_stems failed — surfacing as decode_error",
                    );
                    errors.push(DecodeFailure {
                        deck: *deck,
                        track_id: track.id.clone(),
                        error: e,
                    });
                }
            }
        }
        EventKind::SetStemGain { deck, stem, gain } => {
            // Defensive: silently drop out-of-range stem indices
            // (matches reducer behaviour). Gain is forwarded raw;
            // the audio thread defensive-clamps to [0, 1] on apply.
            if (*stem as usize) < 4 {
                out.push(AudioCommand {
                    at_frame: now_frame,
                    kind: AudioCommandKind::SetStemGain {
                        deck: *deck,
                        stem: *stem,
                        gain: *gain,
                    },
                });
            }
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
        EventKind::EffectSwapSlots {
            deck,
            slot_a,
            slot_b,
        } => {
            // Mirror the reducer's clamping so the audio thread sees
            // the same (a, b) tuple it'd derive from inspecting the
            // resulting state. Same-slot swap → drop the command (no
            // audio side effect; saves a ring slot).
            let last = (next.deck_ref(*deck).effects.len() - 1) as u8;
            let a = (*slot_a).min(last);
            let b = (*slot_b).min(last);
            if a != b {
                out.push(AudioCommand {
                    at_frame: now_frame,
                    kind: AudioCommandKind::EffectSwap { deck: *deck, a, b },
                });
            }
        }
        EventKind::EffectLfoSet { deck, slot, config } => {
            // Forward the post-reducer config so the audio thread + state
            // log agree on the final (clamped) values.
            let resolved = next
                .deck_ref(*deck)
                .effects
                .get(*slot as usize)
                .and_then(|s| s.lfo);
            if let Some(c) = resolved {
                out.push(AudioCommand {
                    at_frame: now_frame,
                    kind: AudioCommandKind::EffectLfoSet {
                        deck: *deck,
                        slot: *slot,
                        config: c,
                    },
                });
            } else {
                // Out-of-range slot — drop silently (mirrors EffectParam path).
                let _ = config;
            }
        }
        EventKind::EffectLfoClear { deck, slot } => {
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::EffectLfoClear {
                    deck: *deck,
                    slot: *slot,
                },
            });
        }
        EventKind::SetMasterLimiterEnabled { .. } => {
            // Read the reducer-finalized value so the audio thread +
            // state log always agree.
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::SetMasterLimiterEnabled {
                    enabled: next.master_limiter_enabled,
                },
            });
        }
        EventKind::SetMasterLimiterThreshold { .. } => {
            // The reducer's already clamped to `[-24, 0]`; forward the
            // post-reducer value so the audio thread + state log agree.
            out.push(AudioCommand {
                at_frame: now_frame,
                kind: AudioCommandKind::SetMasterLimiterThreshold {
                    threshold_db: next.master_limiter_threshold_db,
                },
            });
        }
        EventKind::EffectOneShot { deck, slot, .. } => {
            // Diff-style emit: only push an `EffectEnable { enabled: true }`
            // when the slot actually transitioned off → on. If the slot
            // was already enabled (one-shot only rescheduled the future
            // disengage), no audio-thread side effect is needed — the
            // disengage will land via a follow-up control-loop sweep
            // (separate PR — audio path doesn't yet read `OneShotState`).
            let prev_enabled = prev
                .deck_ref(*deck)
                .effects
                .get(*slot as usize)
                .map(|s| s.enabled)
                .unwrap_or(false);
            let next_enabled = next
                .deck_ref(*deck)
                .effects
                .get(*slot as usize)
                .map(|s| s.enabled)
                .unwrap_or(false);
            if !prev_enabled && next_enabled {
                out.push(AudioCommand {
                    at_frame: now_frame,
                    kind: AudioCommandKind::EffectEnable {
                        deck: *deck,
                        slot: *slot,
                        enabled: true,
                    },
                });
            }
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
    (out, errors)
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
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::DeckLoad { deck, handle, .. } => {
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
    fn effect_swap_slots_emits_effect_swap_command() {
        // ADR-006 reorder — EffectSwapSlots(0,2) on deck A must
        // translate to one POD EffectSwap audio command with the same
        // indices.
        let prev = EngineState::default();
        let e = ev(
            1,
            EventKind::EffectSwapSlots {
                deck: DeckId::A,
                slot_a: 0,
                slot_b: 2,
            },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::EffectSwap { deck, a, b } => {
                assert_eq!(deck, DeckId::A);
                assert_eq!(a, 0);
                assert_eq!(b, 2);
            }
            other => panic!("expected EffectSwap, got {other:?}"),
        }
    }

    #[test]
    fn effect_swap_slots_same_slot_emits_no_command() {
        // a == b → drop the command so we don't waste a ring slot on
        // a no-op (the reducer also no-ops, so audio + control stay
        // consistent).
        let prev = EngineState::default();
        let e = ev(
            1,
            EventKind::EffectSwapSlots {
                deck: DeckId::A,
                slot_a: 1,
                slot_b: 1,
            },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert!(
            cmds.is_empty(),
            "same-slot EffectSwapSlots must not emit an audio command"
        );
    }

    #[test]
    fn effect_swap_slots_out_of_range_clamps_in_translator() {
        // Mirror the reducer's clamping. slot_a=99 → 2, slot_b=0 stays.
        // Resulting command must carry the post-clamp indices so the
        // audio thread + reducer agree on what happened.
        let prev = EngineState::default();
        let e = ev(
            1,
            EventKind::EffectSwapSlots {
                deck: DeckId::B,
                slot_a: 99,
                slot_b: 0,
            },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::EffectSwap { deck, a, b } => {
                assert_eq!(deck, DeckId::B);
                assert_eq!(a, 2);
                assert_eq!(b, 0);
            }
            other => panic!("expected EffectSwap, got {other:?}"),
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
                    hot_cues: [None; 8],
                    track_gain_db: 0.0,
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

    #[test]
    fn set_master_limiter_enabled_translates_to_audio_command() {
        let prev = EngineState::default();
        let e = ev(1, EventKind::SetMasterLimiterEnabled { enabled: false });
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::SetMasterLimiterEnabled { enabled } => assert!(!enabled),
            other => panic!("expected SetMasterLimiterEnabled, got {other:?}"),
        }
    }

    #[test]
    fn set_master_limiter_threshold_forwards_reducer_clamped_value() {
        // Reducer clamps over-max to MAX (= 0.0 dB); translator must
        // forward the *post-reducer* value, not the raw input.
        let prev = EngineState::default();
        let e = ev(
            1,
            EventKind::SetMasterLimiterThreshold { threshold_db: 12.0 },
        );
        let next = prev.apply(&e);
        let cmds = translate(&prev, &next, &e, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::SetMasterLimiterThreshold { threshold_db } => {
                assert!(
                    (threshold_db - crate::audio::MASTER_LIMITER_MAX_THRESHOLD_DB).abs() < 1e-6,
                    "translator should forward reducer-clamped value, got {threshold_db}",
                );
            }
            other => panic!("expected SetMasterLimiterThreshold, got {other:?}"),
        }
    }

    #[test]
    fn effect_lfo_set_emits_audio_command_with_clamped_config() {
        // ADR-006 — EffectLfoSet event must round-trip through the
        // reducer (depth + target_param clamped) and emit an
        // EffectLfoSet audio command carrying the *post-reducer* config.
        use crate::audio::effects::{LfoConfig, RateDiv, Shape};
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
        // Over-max depth (2.0) + valid target_param (0=cutoff_hz).
        let cfg = LfoConfig::new(Shape::Sine, RateDiv::Quarter, 2.0, 0);
        let set = ev(
            2,
            EventKind::EffectLfoSet {
                deck: DeckId::A,
                slot: 0,
                config: cfg,
            },
        );
        let s2 = s1.apply(&set);
        let cmds = translate(&s1, &s2, &set, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::EffectLfoSet { deck, slot, config } => {
                assert_eq!(deck, DeckId::A);
                assert_eq!(slot, 0);
                // Depth was 2.0 → clamped to 1.0 by the reducer.
                assert!(
                    (config.depth - 1.0).abs() < 1e-6,
                    "depth must be reducer-clamped to 1.0, got {}",
                    config.depth
                );
                assert_eq!(config.target_param, 0);
                assert_eq!(config.shape, Shape::Sine);
                assert_eq!(config.rate_div, RateDiv::Quarter);
            }
            other => panic!("expected EffectLfoSet, got {other:?}"),
        }
    }

    #[test]
    fn effect_lfo_clear_emits_audio_command() {
        use crate::audio::effects::{LfoConfig, RateDiv, Shape};
        let s0 = EngineState::default();
        let assign = ev(
            1,
            EventKind::EffectAssign {
                deck: DeckId::B,
                slot: 1,
                effect_id: 1,
            },
        );
        let s1 = s0.apply(&assign);
        let set = ev(
            2,
            EventKind::EffectLfoSet {
                deck: DeckId::B,
                slot: 1,
                config: LfoConfig::new(Shape::Saw, RateDiv::Beat, 0.7, 0),
            },
        );
        let s2 = s1.apply(&set);
        let clear = ev(
            3,
            EventKind::EffectLfoClear {
                deck: DeckId::B,
                slot: 1,
            },
        );
        let s3 = s2.apply(&clear);
        let cmds = translate(&s2, &s3, &clear, 0);
        assert_eq!(cmds.len(), 1);
        match cmds[0].kind {
            AudioCommandKind::EffectLfoClear { deck, slot } => {
                assert_eq!(deck, DeckId::B);
                assert_eq!(slot, 1);
            }
            other => panic!("expected EffectLfoClear, got {other:?}"),
        }
        // Reducer cleared the slot's lfo field.
        assert!(s3.deck_b.effects[1].lfo.is_none());
    }

    // ---------------------------------------------------------------------
    // decode-error surfacing (engine.decode_error notification plumbing)
    // ---------------------------------------------------------------------

    /// `DecodeService` stub whose `open` always errors with the given
    /// `DecodeError` variant — lets us exercise every category branch of
    /// `DecodeFailure::category` without contriving real symphonia
    /// failures.
    struct AlwaysFailDecode {
        builder: fn() -> DecodeError,
    }

    impl DecodeService for AlwaysFailDecode {
        fn open(
            &self,
            _track: &crate::state::TrackRef,
            _target_sample_rate: u32,
        ) -> Result<crate::audio::DecodeHandle, DecodeError> {
            Err((self.builder)())
        }
        fn read(&self, _: crate::audio::DecodeHandle, _: &mut [f32]) -> usize {
            0
        }
        fn close(&self, _: crate::audio::DecodeHandle) {}
        fn underrun_count(&self) -> u64 {
            0
        }
    }

    fn load_event(deck: DeckId, track_id: &str, path: &str) -> Event {
        ev(
            1,
            EventKind::DeckLoad {
                deck,
                track: TrackRef {
                    id: track_id.into(),
                    path: path.into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        )
    }

    #[test]
    fn deck_load_open_failure_surfaces_decode_error_sidecar() {
        // DeckLoad with an unknown filesystem path → `Io` error;
        // translator must (a) emit zero audio commands, (b) push a
        // single `DecodeFailure` onto the errors sidecar that carries
        // the deck + track id and the `file_not_found` category.
        let svc = crate::audio::SymphoniaDecodeService::new();
        let prev = EngineState::default();
        let e = load_event(DeckId::A, "ghost-id", "/nonexistent/track.wav");
        let next = prev.apply(&e);
        let (cmds, errors) = event_to_commands_with_errors(&prev, &next, &e, 0, SR, &svc);
        assert!(
            cmds.is_empty(),
            "failed open should NOT emit a DeckLoad audio command, got {cmds:?}",
        );
        assert_eq!(errors.len(), 1, "expected exactly one DecodeFailure");
        let f = &errors[0];
        assert_eq!(f.deck, DeckId::A);
        assert_eq!(f.track_id, "ghost-id");
        assert_eq!(f.category(), "file_not_found");
        assert!(matches!(f.error, DecodeError::Io { .. }));
    }

    #[test]
    fn deck_load_success_produces_no_decode_failures() {
        // Happy path: StubDecodeService never errors → errors sidecar
        // empty, single DeckLoad command emitted.
        let svc = StubDecodeService::new();
        let prev = EngineState::default();
        let e = load_event(DeckId::B, "ok-id", "/anywhere.mp3");
        let next = prev.apply(&e);
        let (cmds, errors) = event_to_commands_with_errors(&prev, &next, &e, 0, SR, &svc);
        assert_eq!(cmds.len(), 1, "happy load should emit one DeckLoad");
        assert!(matches!(cmds[0].kind, AudioCommandKind::DeckLoad { .. }));
        assert!(
            errors.is_empty(),
            "no decode failures expected on happy load",
        );
    }

    #[test]
    fn non_load_events_never_populate_decode_errors_sidecar() {
        // Sanity: a stream of non-Load events through a deliberately-
        // failing decode service never adds anything to the errors
        // sidecar because translator only calls `open` on DeckLoad.
        let svc = AlwaysFailDecode {
            builder: || DecodeError::NoFreeSlot,
        };
        let s0 = EngineState::default();
        let events = [
            EventKind::DeckPlay { deck: DeckId::A },
            EventKind::DeckPause { deck: DeckId::A },
            EventKind::Crossfader { value: 0.25 },
            EventKind::EqAdjust {
                deck: DeckId::A,
                band: EqBand::Mid,
                value_db: 2.0,
            },
        ];
        let mut s = s0;
        for (i, kind) in events.iter().enumerate() {
            let e = ev((i + 1) as u64, kind.clone());
            let next = s.apply(&e);
            let (_, errors) = event_to_commands_with_errors(&s, &next, &e, 0, SR, &svc);
            assert!(
                errors.is_empty(),
                "non-load event {kind:?} produced spurious decode error",
            );
            s = next;
        }
    }

    #[test]
    fn decode_failure_category_covers_every_decode_error_variant() {
        // Stable, hand-mapped table from DecodeError → category string.
        // The UI branches on `category` for icons/copy, so adding a
        // new variant must add a row here or the toast falls back to
        // a generic look.
        type Case = (fn() -> DecodeError, &'static str);
        let cases: &[Case] = &[
            (
                || DecodeError::Io {
                    path: "/x".into(),
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
                },
                "file_not_found",
            ),
            (
                || DecodeError::Probe("bad header".into()),
                "format_unsupported",
            ),
            (|| DecodeError::NoTrack, "format_unsupported"),
            (
                || DecodeError::Resampler("rubato init".into()),
                "decoder_error",
            ),
            (|| DecodeError::NoFreeSlot, "resource_exhausted"),
            (
                || DecodeError::UnknownInlineSource("missing-key".into()),
                "unknown_inline_source",
            ),
            (
                || DecodeError::Spawn(std::io::Error::other("spawn fail")),
                "decoder_thread_spawn",
            ),
        ];
        for (build, want) in cases {
            let failure = DecodeFailure {
                deck: DeckId::A,
                track_id: "t".into(),
                error: build(),
            };
            assert_eq!(
                failure.category(),
                *want,
                "category for {:?}",
                failure.error,
            );
        }
    }

    #[test]
    fn event_to_commands_shim_drops_decode_error_sidecar() {
        // Ensure the 1-tuple `event_to_commands` helper preserved for
        // benches / older tests still returns the same `AudioCmdBatch`
        // as the tuple-returning variant (silently dropping the errors
        // sidecar so the old call sites compile unchanged).
        let svc = StubDecodeService::new();
        let prev = EngineState::default();
        let e = load_event(DeckId::A, "p", "/p.mp3");
        let next = prev.apply(&e);
        let cmds_legacy = event_to_commands(&prev, &next, &e, 0, SR, &svc);
        let (cmds_tuple, _) = event_to_commands_with_errors(&prev, &next, &e, 0, SR, &svc);
        assert_eq!(cmds_legacy.len(), cmds_tuple.len());
    }
}
