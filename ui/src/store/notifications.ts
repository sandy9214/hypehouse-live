// Notifications store — surface engine.decode_error toasts in the UI.
//
// The Rust engine fans out `engine.decode_error` JSON-RPC
// notifications from two distinct sources:
//   1. **Open-time** — `DeckLoad` decode failure synchronously
//      raised by `event_to_commands_with_errors` in
//      `engine/src/main.rs`.
//   2. **Mid-stream** — decoder thread panics or stream-time
//      decode failures emitted asynchronously through
//      `engine/src/bridge/decode_drain.rs` via
//      `EngineHandle::publish_decode_error`.
// This store buffers a small queue of recent errors, exposes them
// via `useSyncExternalStore`, and auto-evicts after a configurable
// window so the UI stays uncluttered.
//
// Why not roll the queue into `engine.ts`?
//   * Decode errors are NOT part of the event-sourced state — they're
//     transient out-of-band signals. Storing them on `EngineState`
//     would muddy the reducer contract.
//   * The queue has its own eviction policy (timer-driven), which
//     wouldn't fit the pure-reducer pattern.
//   * Tests can reset the toast queue in isolation without touching
//     the live deck mirror.

import { useSyncExternalStore } from "react";
import type { JsonRpcNotification } from "../ws/client";

export type DeckId = "A" | "B";

/**
 * Coarse failure-category strings emitted by the engine. The wire
 * value is a free-form string; we keep the union open via `string` so
 * a new engine release can introduce categories without breaking the
 * UI bundle.
 */
export type DecodeErrorCategory =
  | "file_not_found"
  | "format_unsupported"
  | "decoder_error"
  | "resource_exhausted"
  | "unknown_inline_source"
  | "decoder_thread_spawn"
  // Mid-stream failures observed AFTER the decoder thread spawned —
  // symphonia decode error / rubato resample error mid-track. The
  // engine continues to silence-pad the ring; this toast tells the
  // operator why the deck went quiet.
  | "mid_stream_decode_failure"
  // Decoder thread itself panicked and the engine's `catch_unwind`
  // guard caught the unwind. The audio thread keeps running; the
  // affected track is dead but other decks are unharmed.
  | "decoder_thread_panic"
  | (string & { readonly __brand?: never });

/**
 * A single in-flight decode-error toast. `id` is a process-monotonic
 * counter assigned on receipt — `track_id` isn't unique enough (the
 * user might retry the same load twice and we want both visible).
 */
export interface DecodeErrorNotification {
  readonly id: number;
  readonly deck: DeckId;
  readonly track_id: string;
  readonly category: DecodeErrorCategory;
  readonly error: string;
  /** Process-local wall time (ms) the engine notification was received. */
  readonly received_at_ms: number;
}

/**
 * Auto-dismiss window in milliseconds. Per the PR spec a decode-error
 * toast lives for 5 seconds. Exported so the Toaster component can
 * align its CSS transitions to the same number without drifting.
 */
export const DECODE_ERROR_AUTO_DISMISS_MS = 5_000;

/**
 * Hard cap on queue size to defend against a runaway engine spamming
 * errors. The Toaster only ever renders the most recent N (default 3),
 * but the queue holds a few extras so a rapid burst doesn't lose
 * intermediate context before the operator sees it.
 */
const MAX_QUEUE = 16;

type Listener = () => void;

let nextId = 1;
let current: ReadonlyArray<DecodeErrorNotification> = [];
const listeners = new Set<Listener>();
const timers = new Map<number, ReturnType<typeof setTimeout>>();

const subscribeStore = (l: Listener): (() => void) => {
  listeners.add(l);
  return (): void => {
    listeners.delete(l);
  };
};

const getSnapshot = (): ReadonlyArray<DecodeErrorNotification> => current;

const notifyListeners = (): void => {
  for (const l of listeners) l();
};

interface DecodeErrorPayload {
  deck?: DeckId;
  track_id?: string;
  category?: string;
  error?: string;
}

/**
 * Apply a single server-pushed notification. Only `engine.decode_error`
 * is consumed; unknown methods are ignored so this handler can be
 * registered on the same `client.subscribe` slot as the engine-state
 * handler without conflict.
 *
 * Each accepted error is appended to the queue AND scheduled for
 * auto-dismiss `DECODE_ERROR_AUTO_DISMISS_MS` later. Tests can pin
 * `Date.now` / use fake timers to assert deterministic behaviour.
 */
export const applyDecodeErrorNotification = (n: JsonRpcNotification): void => {
  if (n.method !== "engine.decode_error") return;
  const p = (n.params ?? {}) as DecodeErrorPayload;
  if (
    p.deck !== "A" &&
    p.deck !== "B"
  ) {
    return;
  }
  if (typeof p.track_id !== "string") return;
  if (typeof p.error !== "string") return;
  const id = nextId++;
  const next: DecodeErrorNotification = {
    id,
    deck: p.deck,
    track_id: p.track_id,
    category: (typeof p.category === "string" && p.category.length > 0
      ? p.category
      : "decoder_error") as DecodeErrorCategory,
    error: p.error,
    received_at_ms: Date.now(),
  };
  // Append + cap.
  const appended = current.length >= MAX_QUEUE
    ? [...current.slice(current.length - MAX_QUEUE + 1), next]
    : [...current, next];
  current = appended;
  // Schedule auto-dismiss. `setTimeout` returns a Timer / number depending
  // on the runtime — keep the typed union and clear via the same handle.
  const handle = setTimeout((): void => {
    dismissDecodeError(id);
  }, DECODE_ERROR_AUTO_DISMISS_MS);
  timers.set(id, handle);
  notifyListeners();
};

/** Drop a single error by id. Idempotent (no-op if already gone). */
export const dismissDecodeError = (id: number): void => {
  const handle = timers.get(id);
  if (handle !== undefined) {
    clearTimeout(handle);
    timers.delete(id);
  }
  const next = current.filter((e): boolean => e.id !== id);
  if (next.length !== current.length) {
    current = next;
    notifyListeners();
  }
};

/** React hook returning the live queue of decode-error toasts. */
export const useDecodeErrors = (): ReadonlyArray<DecodeErrorNotification> =>
  useSyncExternalStore(subscribeStore, getSnapshot, getSnapshot);

/**
 * Test-only reset hook. Clears the queue + cancels every pending
 * auto-dismiss timer so an `afterEach` can leave the store in a known
 * state between tests.
 */
export const __resetDecodeErrors = (): void => {
  for (const handle of timers.values()) {
    clearTimeout(handle);
  }
  timers.clear();
  current = [];
  nextId = 1;
  notifyListeners();
};
