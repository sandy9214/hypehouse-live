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
  hot_cues: ReadonlyArray<number | null>; // length 8
  loop_in_ms: number | null;
  loop_out_ms: number | null;
  copilot_enabled: boolean;
}

export interface EngineState {
  decks: readonly [Deck, Deck];
  crossfader: number;
  last_event_id: number;
}

type Listener = () => void;

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
  hot_cues: [null, null, null, null, null, null, null, null],
  loop_in_ms: null,
  loop_out_ms: null,
  copilot_enabled: false,
});

let current: EngineState = {
  decks: [emptyDeck("A"), emptyDeck("B")],
  crossfader: 0.5,
  last_event_id: 0,
};
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
 * Apply a server-pushed notification. Only `engine.state_changed`
 * mutates local state today. Unknown methods are silently ignored.
 */
export const applyNotification = (n: JsonRpcNotification): void => {
  if (n.method !== "engine.state_changed") return;
  const params = n.params as { state?: Partial<EngineState> } | undefined;
  if (!params || !params.state) return;
  const next = mergeState(current, params.state);
  if (next !== current) {
    current = next;
    notifyListeners();
  }
};

const mergeState = (
  prev: EngineState,
  patch: Partial<EngineState>,
): EngineState => {
  const decks = patch.decks
    ? ([patch.decks[0] ?? prev.decks[0], patch.decks[1] ?? prev.decks[1]] as [
        Deck,
        Deck,
      ])
    : prev.decks;
  return {
    decks,
    crossfader: patch.crossfader ?? prev.crossfader,
    last_event_id: patch.last_event_id ?? prev.last_event_id,
  };
};

/** React hook returning the current engine state snapshot. */
export const useEngineState = (): EngineState =>
  useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

/** Test/internal hook — reset back to empty state. */
export const __resetEngineState = (): void => {
  current = {
    decks: [emptyDeck("A"), emptyDeck("B")],
    crossfader: 0.5,
    last_event_id: 0,
  };
  notifyListeners();
};
