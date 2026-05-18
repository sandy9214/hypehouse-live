// Auto-mix store — opt-in transition execution per deck.
//
// The copilot service (Python) exposes two JSON-RPC methods:
//   * copilot.set_auto_mix({deck, enabled}) — opt a deck in/out.
//   * copilot.get_auto_mix({deck})          — fetch current state.
//
// AND a push notification `copilot.auto_mix_state_changed` that fires
// whenever the state machine advances (IDLE → ARMED → TRANSITIONING →
// DONE → IDLE). The UI subscribes to that notification stream and
// mirrors the latest snapshot per-deck for the Auto-Mix toggle button
// + countdown indicator.
//
// Why a dedicated store (and not state on Deck.tsx)?
//   * The notification stream is global; mirroring it in component
//     state would force every Deck render to fan out the subscription
//     itself.
//   * Reset semantics differ from engine state — auto-mix state lives
//     entirely in copilot memory; the engine doesn't echo it back. We
//     need an independent eviction path on copilot reconnect.

import { useEffect, useSyncExternalStore } from "react";
import type { JsonRpcNotification, JsonRpcWS } from "../ws/client";

export type DeckId = "A" | "B";

/** Wire-shape values the copilot pushes in `copilot.auto_mix_state_changed`. */
export type AutoMixStatus = "idle" | "armed" | "transitioning" | "done";

/**
 * Per-deck snapshot the `useAutoMix(deck)` hook returns. `enabled` is
 * the operator's opt-in flag; `status` is the state-machine label;
 * `seconds_to_mix` is the live countdown (null when outside the
 * look-ahead window or no track loaded).
 */
export interface AutoMixSnapshot {
  readonly enabled: boolean;
  readonly status: AutoMixStatus;
  readonly seconds_to_mix: number | null;
}

const DEFAULT_SNAPSHOT: AutoMixSnapshot = Object.freeze({
  enabled: false,
  status: "idle",
  seconds_to_mix: null,
});

type Listener = () => void;

// One snapshot per deck. Using a plain Record keyed on deck id keeps
// the comparison fast: useSyncExternalStore re-renders when the
// per-deck reference changes, NOT when the parent map changes.
let current: Readonly<Record<DeckId, AutoMixSnapshot>> = {
  A: DEFAULT_SNAPSHOT,
  B: DEFAULT_SNAPSHOT,
};
const listeners = new Set<Listener>();

const notifyListeners = (): void => {
  for (const l of listeners) l();
};

const subscribeStore = (l: Listener): (() => void) => {
  listeners.add(l);
  return (): void => {
    listeners.delete(l);
  };
};

const getDeckSnapshot = (deck: DeckId): AutoMixSnapshot => current[deck];

interface AutoMixStateChangedPayload {
  deck?: DeckId;
  status?: AutoMixStatus;
  seconds_to_mix?: number | null;
  enabled?: boolean;
}

/**
 * Apply one server-pushed `copilot.auto_mix_state_changed`
 * notification. Unknown methods are ignored so this can sit on the
 * same notification-subscription slot as `applyDecodeErrorNotification`
 * without conflict.
 *
 * The copilot's wire shape doesn't include `enabled` in every
 * notification (it only fires on state-machine advances, not toggle
 * changes). We preserve the previous `enabled` value across
 * notifications that omit it — the operator's flag is sticky.
 */
export const applyAutoMixNotification = (n: JsonRpcNotification): void => {
  if (n.method !== "copilot.auto_mix_state_changed") return;
  const p = (n.params ?? {}) as AutoMixStateChangedPayload;
  if (p.deck !== "A" && p.deck !== "B") return;
  if (
    p.status !== "idle" &&
    p.status !== "armed" &&
    p.status !== "transitioning" &&
    p.status !== "done"
  ) {
    return;
  }
  const prev = current[p.deck];
  // `seconds_to_mix` is allowed to be null (clears countdown). We
  // normalize `undefined` → null so the UI doesn't need to branch.
  const seconds =
    typeof p.seconds_to_mix === "number" ? p.seconds_to_mix : null;
  const next: AutoMixSnapshot = {
    enabled: typeof p.enabled === "boolean" ? p.enabled : prev.enabled,
    status: p.status,
    seconds_to_mix: seconds,
  };
  // Skip the re-render churn if nothing actually changed.
  if (
    prev.enabled === next.enabled &&
    prev.status === next.status &&
    prev.seconds_to_mix === next.seconds_to_mix
  ) {
    return;
  }
  current = { ...current, [p.deck]: next };
  notifyListeners();
};

/**
 * Mutate the local snapshot directly. Used by the `setAutoMix` RPC
 * call below so the UI updates optimistically while the copilot's
 * notification round-trip is in flight.
 */
const applyLocalUpdate = (
  deck: DeckId,
  next: Partial<AutoMixSnapshot>,
): void => {
  const prev = current[deck];
  const merged: AutoMixSnapshot = {
    enabled: next.enabled ?? prev.enabled,
    status: next.status ?? prev.status,
    seconds_to_mix:
      next.seconds_to_mix !== undefined
        ? next.seconds_to_mix
        : prev.seconds_to_mix,
  };
  if (
    prev.enabled === merged.enabled &&
    prev.status === merged.status &&
    prev.seconds_to_mix === merged.seconds_to_mix
  ) {
    return;
  }
  current = { ...current, [deck]: merged };
  notifyListeners();
};

/**
 * Fire `copilot.set_auto_mix({deck, enabled})` and optimistically
 * update the local snapshot. The copilot's notification (when it
 * arrives) will refine the status; this just keeps the button state
 * responsive.
 */
export const setAutoMix = async (
  client: JsonRpcWS,
  deck: DeckId,
  enabled: boolean,
): Promise<void> => {
  applyLocalUpdate(deck, { enabled });
  try {
    await client.call("copilot.set_auto_mix", { deck, enabled });
  } catch {
    // Roll back the optimistic flag on RPC failure so the UI doesn't
    // lie about the copilot's actual state. The button will re-arm
    // on the operator's next click.
    applyLocalUpdate(deck, { enabled: !enabled });
  }
};

/**
 * React hook returning the live auto-mix snapshot for `deck`. Wires
 * itself into the copilot's notification stream on first mount; the
 * subscription is shared so multiple components mounting `useAutoMix`
 * incur zero per-component cost beyond the snapshot read.
 */
export const useAutoMix = (deck: DeckId): AutoMixSnapshot =>
  useSyncExternalStore(
    subscribeStore,
    (): AutoMixSnapshot => getDeckSnapshot(deck),
    (): AutoMixSnapshot => getDeckSnapshot(deck),
  );

/**
 * Convenience: register the copilot notification handler against a
 * `JsonRpcWS` client. Returns the unsubscribe callable. Usually wired
 * once at App-mount alongside `applyDecodeErrorNotification`.
 */
export const subscribeAutoMix = (client: JsonRpcWS): (() => void) =>
  client.subscribe(applyAutoMixNotification);

/**
 * React-friendly variant of `subscribeAutoMix` — call from a top-level
 * component's `useEffect` to wire the notification stream. Returns
 * void; the cleanup runs on unmount.
 */
export const useAutoMixSubscription = (client: JsonRpcWS): void => {
  useEffect((): (() => void) => {
    const unsub = subscribeAutoMix(client);
    return (): void => {
      unsub();
    };
  }, [client]);
};

/**
 * Test-only reset. Clears all per-deck state and notifies subscribers
 * so an `afterEach` can leave the store in a known state.
 */
export const __resetAutoMix = (): void => {
  current = { A: DEFAULT_SNAPSHOT, B: DEFAULT_SNAPSHOT };
  notifyListeners();
};
