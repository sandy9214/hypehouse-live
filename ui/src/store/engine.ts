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
  notifyListeners();
};
