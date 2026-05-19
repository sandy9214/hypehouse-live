// Tiny client-side mirror of the engine state.
//
// The Rust engine is the source of truth (see engine/src/state.rs and
// docs/api/ws-protocol.md ADR-001/003). The UI keeps a shallow copy
// updated from `engine.state_changed` notifications and exposes it via
// `useSyncExternalStore` so React 18 sees consistent snapshots without
// `useState` ping-pong inside components.

import { useSyncExternalStore } from "react";
import type { JsonRpcNotification } from "../ws/client";

export type DeckId = "A" | "B";

/**
 * Active tempo source for the master clock. Mirrors the engine-side
 * `audio::clock::ClockSource` enum (engine/src/audio/clock.rs). Wire
 * label is the stable kebab-case string the engine emits on every
 * `engine.state_changed` payload as a sibling of `state`:
 *   - `"internal"`     — engine drives its own master_bpm (default).
 *   - `"midi_in"`      — external MIDI clock master is locked in.
 *   - `"ableton_link"` — peer session is driving the tempo (future).
 *
 * Keep this union in lockstep with `ClockSource::as_str` on the engine
 * side; mismatches collapse to `"internal"` via `normaliseClockSource`.
 */
export type ClockSource = "internal" | "midi_in" | "ableton_link";

const VALID_CLOCK_SOURCES: ReadonlyArray<ClockSource> = [
  "internal",
  "midi_in",
  "ableton_link",
];

/**
 * Crossfader response curve — mirrors the engine's `CrossfaderCurve`
 * enum (engine/src/state.rs). The full `EngineState` snapshot serializes
 * this field, so it rides on every `engine.state_changed`. Defaulted
 * to `"Linear"` on an empty mirror to match the engine's `Default`
 * impl. Used by the preset panel to capture the current scene.
 */
export type CrossfaderCurve = "Linear" | "Dipped" | "Sharp" | "Scratch";

const VALID_CROSSFADER_CURVES: ReadonlyArray<CrossfaderCurve> = [
  "Linear",
  "Dipped",
  "Sharp",
  "Scratch",
];

const normaliseCrossfaderCurve = (raw: unknown): CrossfaderCurve => {
  if (
    typeof raw === "string" &&
    (VALID_CROSSFADER_CURVES as string[]).includes(raw)
  ) {
    return raw as CrossfaderCurve;
  }
  return "Linear";
};

/** Coerce an unknown wire value to a known `ClockSource`. Defends the
 * mirror against a future engine that ships a variant the UI doesn't
 * recognise yet — unknown source = treat as `internal` (badge falls
 * back to the no-lock state rather than glitching). */
const normaliseClockSource = (raw: unknown): ClockSource => {
  if (typeof raw === "string" && (VALID_CLOCK_SOURCES as string[]).includes(raw)) {
    return raw as ClockSource;
  }
  return "internal";
};

/**
 * Single effect slot mirror of engine `EffectSlot` (engine/src/state.rs).
 * `effect_id` = 0 means empty. `params` is a string→number map keyed
 * by the param descriptor's `name`.
 */
/**
 * Beat-FX one-shot scheduled disengage. Mirrors `engine::state::OneShotState`.
 * `ends_at_micros` is wall-clock micros (since UNIX epoch, same scale as
 * the engine's `Event.ts_micros`). UI countdown renders the remaining
 * window as `ends_at_micros - now_micros`; engine wall clock is the
 * authoritative source (no need to recompute against current beat_period_ms,
 * which can mutate mid-flight — see issue #128).
 */
export interface OneShotState {
  ends_at_micros: number;
  was_enabled: boolean;
  /**
   * Beat period (ms) frozen at the moment of dispatch. UI countdowns
   * divide remaining-ms by this value (not the deck's live
   * `beat_period_ms`, which can mutate mid-flight on a grid retune).
   * Issue #128. Optional for wire-compat with older snapshots.
   */
  beat_period_ms_at_dispatch?: number;
}

export interface EffectSlotState {
  effect_id: number;
  params: Readonly<Record<string, number>>;
  wet_dry: number;
  enabled: boolean;
  /** `null` / undefined when no one-shot is active. */
  one_shot?: OneShotState | null;
}

export interface Deck {
  id: DeckId;
  track_title: string | null;
  bpm: number | null;
  position_ms: number;
  playing: boolean;
  eq_low: number;
  eq_mid: number;
  eq_high: number;
  pitch_semitones: number;
  /**
   * Tempo ratio — 1.0 = original speed, < 1 = slower, > 1 = faster.
   * Independent of `pitch_semitones` (post the pitch/tempo-independent
   * PR). Range clamped engine-side to `[0.5, 2.0]`. Mirrors the
   * `Deck::tempo_ratio` field in `engine/src/state.rs`.
   */
  tempo_ratio: number;
  hot_cues: ReadonlyArray<number | null>; // length 8
  loop_in_ms: number | null;
  loop_out_ms: number | null;
  /**
   * `true` once both `loop_in_ms` and `loop_out_ms` are set + the audio
   * thread has armed the loop. Mirror of `Deck::loop_active` in
   * `engine/src/state.rs`. The bar-preset row uses this to highlight
   * the currently-active preset. Defaults to `false` so a fresh deck
   * renders with no preset highlighted.
   */
  loop_active: boolean;
  /**
   * Milliseconds per beat — mirror of `Deck::beat_period_ms`. Used by
   * the bar-preset row to derive **which** preset (1/2/4/8/16) matches
   * the current `loop_out_ms - loop_in_ms` length so the active button
   * can highlight without a separate echo-back from the engine.
   * `0` when no track is loaded (engine serializes `Deck::beat_period_ms`
   * as `f32`; a fresh deck has `0.0`). Consumers should treat any
   * non-positive value as "no beat grid yet".
   */
  beat_period_ms: number;
  copilot_enabled: boolean;
  /** Per-deck effects chain (ADR-006). Length 3. */
  effects: readonly [EffectSlotState, EffectSlotState, EffectSlotState];
  /**
   * Per-stem linear gain when the deck is loaded with separated stems
   * via `EventKind::DeckLoadStems`. Indexed canonically —
   * `0=vocals`, `1=drums`, `2=bass`, `3=other`. Mirror of
   * `Deck::stem_gains` in `engine/src/state.rs`. Default
   * `[1, 1, 1, 1]` so the UI mute toggles render in the "on" state
   * before the engine has confirmed a stem load.
   */
  stem_gains: readonly [number, number, number, number];
  /**
   * `true` after a successful `DeckLoadStems` reducer pass — the
   * `StemRack` mute controls activate only in this mode. Mirror of
   * `Deck::stem_mode` in `engine/src/state.rs`. A subsequent
   * `DeckLoad` clears it (mutually exclusive with full-mix playback).
   */
  stem_mode: boolean;
}

/**
 * Sidechain compressor config (issue #119). Mirrors engine
 * `state::SidechainConfig`. Defaults set engine-side.
 */
export interface SidechainConfig {
  enabled: boolean;
  trigger_deck: DeckId;
  threshold_db: number;
  ratio: number;
  attack_ms: number;
  release_ms: number;
  makeup_gain_db: number;
}

export const DEFAULT_SIDECHAIN: SidechainConfig = {
  enabled: false,
  trigger_deck: "A",
  threshold_db: -12,
  ratio: 4,
  attack_ms: 5,
  release_ms: 200,
  makeup_gain_db: 0,
};

export interface EngineState {
  decks: readonly [Deck, Deck];
  crossfader: number;
  /**
   * Mirror of `EngineState::crossfader_curve` (engine/src/state.rs).
   * The engine broadcasts the field as part of every `engine.state_changed`
   * snapshot. Defaults to `"Linear"` so a pre-this-PR engine that omits
   * the field still produces a sensible UI mirror. Used by the preset
   * panel to capture / restore the curve as part of a scene.
   */
  crossfader_curve: CrossfaderCurve;
  last_event_id: number;
  /**
   * Master-bus soft-clip limiter — toggle. Mirror of
   * `EngineState::master_limiter_enabled` in `engine/src/state.rs`.
   * Default `true` so a fresh session has the safety net armed.
   */
  master_limiter_enabled: boolean;
  /**
   * Master-bus soft-clip limiter — threshold in dB. Mirror of
   * `EngineState::master_limiter_threshold_db`. Engine-side clamp is
   * `[-24.0, 0.0]`; the UI knob enforces the same window.
   */
  master_limiter_threshold_db: number;
  /**
   * Sidechain compressor config (issue #119). Mirror of
   * `EngineState.sidechain` on the engine side. `#[serde(default)]`
   * on the engine — older snapshots without the field deserialize to
   * a default-disabled config. Optional on the UI side for the same
   * wire-compat reason.
   */
  sidechain?: SidechainConfig;
  /**
   * Live master-bus limiter gain reduction in dB at the moment the
   * last `engine.state_changed` notification was published. Always
   * `<= 0`. Sourced from a side-channel atomic on the audio thread
   * (NOT part of the event-sourced state). Drives the
   * `MasterControls` vertical meter.
   */
  master_limiter_gain_reduction_db: number;
  /**
   * Active tempo source at the moment the last `engine.state_changed`
   * notification was published. Like `master_limiter_gain_reduction_db`
   * this is a live audio-thread measurement (sourced from the engine's
   * `SharedClock` atomic) and rides on the envelope rather than inside
   * `state`. Drives the BPM-lock badge in `MasterControls`.
   */
  clock_source: ClockSource;
}

type Listener = () => void;

const emptyEffectSlot = (): EffectSlotState => ({
  effect_id: 0,
  params: {},
  wet_dry: 0.5,
  enabled: false,
});

const emptyDeck = (id: DeckId): Deck => ({
  id,
  track_title: null,
  bpm: null,
  position_ms: 0,
  playing: false,
  eq_low: 0,
  eq_mid: 0,
  eq_high: 0,
  pitch_semitones: 0,
  tempo_ratio: 1.0,
  hot_cues: [null, null, null, null, null, null, null, null],
  loop_in_ms: null,
  loop_out_ms: null,
  loop_active: false,
  beat_period_ms: 0,
  copilot_enabled: false,
  effects: [emptyEffectSlot(), emptyEffectSlot(), emptyEffectSlot()],
  stem_gains: [1, 1, 1, 1],
  stem_mode: false,
});

/**
 * Default master-bus limiter threshold in dB. Mirrors
 * `audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB` (engine-side). Kept here
 * so an empty mirror exposes the same defaults a fresh engine session
 * would broadcast on the first `state_changed`.
 */
const DEFAULT_MASTER_LIMITER_THRESHOLD_DB = -0.5;

const emptyEngineState = (): EngineState => ({
  decks: [emptyDeck("A"), emptyDeck("B")],
  crossfader: 0.5,
  crossfader_curve: "Linear",
  last_event_id: 0,
  master_limiter_enabled: true,
  master_limiter_threshold_db: DEFAULT_MASTER_LIMITER_THRESHOLD_DB,
  master_limiter_gain_reduction_db: 0,
  clock_source: "internal",
  sidechain: DEFAULT_SIDECHAIN,
});

let current: EngineState = emptyEngineState();
const listeners = new Set<Listener>();

/**
 * Per-deck record of the last `engine.state_changed` push for a given
 * deck — wall-clock timestamp + the `position_ms` we received. The
 * rAF-driven waveform extrapolates between server pushes using:
 *   pos_now ≈ last_position_ms + (now - last_update_ts) × tempo_ratio
 *
 * Why two records (not one): each deck advances independently; deck A
 * might be paused while deck B is playing. Server pushes are sparse
 * (~5 Hz) but rAF runs ~60 Hz, so extrapolation hides the gap.
 *
 * Track-id keyed: we'd ideally re-zero on track change, but the engine
 * state mirror has no track_id field. Caller (Deck.tsx) clears the
 * provider when the deck unloads, so a stale record never feeds the
 * visualiser.
 */
interface PositionAnchor {
  positionMs: number;
  updateTs: number;
  durationMs: number;
  tempoRatio: number;
  playing: boolean;
}
const positionAnchors = new Map<DeckId, PositionAnchor>();

/** Override for `now()` so tests can advance time deterministically. */
let nowFn: () => number = (): number => Date.now();
export const __setNowForTest = (fn: () => number): void => {
  nowFn = fn;
};
export const __resetNowForTest = (): void => {
  nowFn = (): number => Date.now();
};

const updateAnchor = (deck: Deck, durationMs: number): void => {
  positionAnchors.set(deck.id, {
    positionMs: deck.position_ms,
    updateTs: nowFn(),
    durationMs,
    tempoRatio: deck.tempo_ratio,
    playing: deck.playing,
  });
};

/**
 * Smoothed playhead position for `deckId`. Returns the last known
 * `position_ms` advanced by `(now - last_update_ts) × tempo_ratio` —
 * unless the deck is paused, in which case the static position is
 * returned. Clamped to `[0, durationMs]` so the playhead never falls
 * off the visible end.
 *
 * `durationMsHint` overrides the anchor's stored duration — covers the
 * common case where the UI has duration from the LibraryTrack payload
 * but the anchor was set before the deck knew its duration.
 */
export const extrapolatedPosition = (
  deckId: DeckId,
  durationMsHint?: number,
): number => {
  const anchor = positionAnchors.get(deckId);
  if (!anchor) return 0;
  const duration =
    typeof durationMsHint === "number" && durationMsHint > 0
      ? durationMsHint
      : anchor.durationMs;
  if (!anchor.playing) {
    return duration > 0
      ? Math.max(0, Math.min(anchor.positionMs, duration))
      : Math.max(0, anchor.positionMs);
  }
  const dtMs = Math.max(0, nowFn() - anchor.updateTs);
  const advanced = anchor.positionMs + dtMs * anchor.tempoRatio;
  if (duration > 0) return Math.max(0, Math.min(advanced, duration));
  return Math.max(0, advanced);
};

/** Reset position anchors (test/internal). */
export const __resetPositionAnchors = (): void => positionAnchors.clear();

const subscribe = (l: Listener): (() => void) => {
  listeners.add(l);
  return (): void => {
    listeners.delete(l);
  };
};

const getSnapshot = (): EngineState => current;

const notifyListeners = (): void => {
  for (const l of listeners) l();
};

/**
 * Wire-side shape of an `engine.state_changed` notification payload.
 * The engine sends `state` (the full `EngineState` snapshot) alongside
 * `last_event_id` and the side-channel `master_limiter_gain_reduction_db`
 * (sampled off the audio thread's atomic). The GR field lives **outside**
 * `state` because gain reduction is a live measurement, not part of the
 * event-sourced reducer state — keeping it on the envelope avoids
 * polluting the reducer snapshot with a non-deterministic value.
 */
interface StateChangedPayload {
  state?: Partial<EngineState>;
  last_event_id?: number;
  master_limiter_gain_reduction_db?: number;
  /** Sibling field — see `ClockSource` jsdoc. Engine emits the
   * kebab-case label; `normaliseClockSource` defends the mirror
   * against unknown variants. */
  clock_source?: unknown;
}

/**
 * Apply a server-pushed notification. Only `engine.state_changed`
 * mutates local state today. Unknown methods are silently ignored.
 */
export const applyNotification = (n: JsonRpcNotification): void => {
  if (n.method !== "engine.state_changed") return;
  const params = n.params as StateChangedPayload | undefined;
  if (!params || !params.state) return;
  const next = mergeState(current, params);
  // Refresh position anchors on every push — even if the merged state is
  // shallow-equal we still want the timestamp to advance so future
  // extrapolation re-bases off the latest server-reported position.
  for (const d of next.decks) {
    const anchor = positionAnchors.get(d.id);
    updateAnchor(d, anchor?.durationMs ?? 0);
  }
  if (next !== current) {
    current = next;
    notifyListeners();
  }
};

const mergeState = (
  prev: EngineState,
  payload: StateChangedPayload,
): EngineState => {
  const patch = payload.state ?? {};
  const decks = patch.decks
    ? ([patch.decks[0] ?? prev.decks[0], patch.decks[1] ?? prev.decks[1]] as [
        Deck,
        Deck,
      ])
    : prev.decks;
  // `master_limiter_gain_reduction_db` rides on the envelope, not `state`.
  // Fall back to prev when the engine omits it (old snapshots / replay).
  const gr =
    typeof payload.master_limiter_gain_reduction_db === "number"
      ? payload.master_limiter_gain_reduction_db
      : prev.master_limiter_gain_reduction_db;
  // `clock_source` also rides on the envelope. Omitted = stick with
  // previous value so older engines (pre-this-PR) don't reset the
  // badge on every push.
  const source =
    payload.clock_source === undefined
      ? prev.clock_source
      : normaliseClockSource(payload.clock_source);
  // `crossfader_curve` rides inside the `state` patch (it's part of the
  // engine's serialised `EngineState`). Engines that omit the field
  // (pre-curve-PR snapshots, replays of older event logs) keep the
  // prior value rather than thrash back to Linear on every push.
  const curve =
    patch.crossfader_curve === undefined
      ? prev.crossfader_curve
      : normaliseCrossfaderCurve(patch.crossfader_curve);
  return {
    decks,
    crossfader: patch.crossfader ?? prev.crossfader,
    crossfader_curve: curve,
    last_event_id:
      payload.last_event_id ?? patch.last_event_id ?? prev.last_event_id,
    master_limiter_enabled:
      patch.master_limiter_enabled ?? prev.master_limiter_enabled,
    master_limiter_threshold_db:
      patch.master_limiter_threshold_db ?? prev.master_limiter_threshold_db,
    master_limiter_gain_reduction_db: gr,
    clock_source: source,
  };
};

/** React hook returning the current engine state snapshot. */
export const useEngineState = (): EngineState =>
  useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

/** Test/internal hook — reset back to empty state. */
export const __resetEngineState = (): void => {
  current = emptyEngineState();
  positionAnchors.clear();
  notifyListeners();
};

/**
 * Public anchor helper for components that own a deck's duration (the
 * engine state mirror doesn't carry it — see Deck.tsx). Lets Deck.tsx
 * register the duration once at DeckLoad so the clamp inside
 * `extrapolatedPosition` works correctly.
 */
export const setDeckDuration = (deckId: DeckId, durationMs: number): void => {
  const anchor = positionAnchors.get(deckId);
  if (anchor) {
    positionAnchors.set(deckId, { ...anchor, durationMs });
    return;
  }
  // No anchor yet — synthesise one from the current snapshot so a
  // pre-state_changed call from Deck still primes the extrapolator.
  const deck = current.decks.find((d): boolean => d.id === deckId);
  if (!deck) return;
  positionAnchors.set(deckId, {
    positionMs: deck.position_ms,
    updateTs: nowFn(),
    durationMs,
    tempoRatio: deck.tempo_ratio,
    playing: deck.playing,
  });
};
