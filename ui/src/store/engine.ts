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
 * Single effect slot mirror of engine `EffectSlot` (engine/src/state.rs).
 * `effect_id` = 0 means empty. `params` is a string→number map keyed
 * by the param descriptor's `name`.
 */
export interface EffectSlotState {
  effect_id: number;
  params: Readonly<Record<string, number>>;
  wet_dry: number;
  enabled: boolean;
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
  copilot_enabled: boolean;
  /** Per-deck effects chain (ADR-006). Length 3. */
  effects: readonly [EffectSlotState, EffectSlotState, EffectSlotState];
}

export interface EngineState {
  decks: readonly [Deck, Deck];
  crossfader: number;
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
   * Live master-bus limiter gain reduction in dB at the moment the
   * last `engine.state_changed` notification was published. Always
   * `<= 0`. Sourced from a side-channel atomic on the audio thread
   * (NOT part of the event-sourced state). Drives the
   * `MasterControls` vertical meter.
   */
  master_limiter_gain_reduction_db: number;
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
  copilot_enabled: false,
  effects: [emptyEffectSlot(), emptyEffectSlot(), emptyEffectSlot()],
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
  last_event_id: 0,
  master_limiter_enabled: true,
  master_limiter_threshold_db: DEFAULT_MASTER_LIMITER_THRESHOLD_DB,
  master_limiter_gain_reduction_db: 0,
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
  return {
    decks,
    crossfader: patch.crossfader ?? prev.crossfader,
    last_event_id:
      payload.last_event_id ?? patch.last_event_id ?? prev.last_event_id,
    master_limiter_enabled:
      patch.master_limiter_enabled ?? prev.master_limiter_enabled,
    master_limiter_threshold_db:
      patch.master_limiter_threshold_db ?? prev.master_limiter_threshold_db,
    master_limiter_gain_reduction_db: gr,
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
