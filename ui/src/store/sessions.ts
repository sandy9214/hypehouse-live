// Sessions history store.
//
// Mirrors the shape returned by the engine's `engine.list_sessions` and
// `engine.replay_session` RPCs (see `docs/api/ws-protocol.md` + the Rust
// types in `engine/src/persistence/sessions.rs`).
//
// Two surfaces:
//   * `useSessions(client)` — fetches + caches the list of past sessions.
//     Subscribes via `useSyncExternalStore` so React 18 sees stable
//     snapshots without per-component state ping-pong.
//   * `useReplay(client, sessionId)` — fetches the replayed
//     `EngineState` snapshot for a single session. Caches the last few
//     replay results in an LRU so toggling between two recent sessions
//     in the UI doesn't re-fetch from the engine every click.
//
// The replay payload uses the engine's wire shape verbatim — the UI's
// existing `store/engine.ts` types model the live state, but the
// replayed state has every field the engine emits (limiter,
// master_volume_db, etc.) which the live store flattens. We model a
// permissive `ReplayedEngineState` here so the History panel can show
// whatever the engine ships without forcing the live store to widen.

import { useCallback, useEffect, useSyncExternalStore } from "react";
import type { JsonRpcWS } from "../ws/client";

/** One row in the sessions list — matches `SessionSummary` in Rust. */
export interface SessionSummary {
  readonly id: string;
  readonly started_at_micros: number | null;
  readonly ended_at_micros: number | null;
  readonly event_count: number;
  readonly has_recording: boolean;
  readonly recording_size_bytes: number | null;
}

/**
 * Shape of one replayed `EngineState` snapshot. Modeled as
 * `Record<string, unknown>` because the engine state shape is a moving
 * target (effects, limiter, master volume) and the History panel only
 * pretty-prints a handful of well-known fields. The accessor helpers
 * below are how the component pulls structured values out without
 * widening the type.
 */
export type ReplayedEngineState = Readonly<Record<string, unknown>>;

export interface ReplayResult {
  readonly state: ReplayedEngineState;
  readonly event_count: number;
}

interface SessionsStoreState {
  readonly sessions: ReadonlyArray<SessionSummary>;
  readonly loaded: boolean;
  readonly loading: boolean;
  readonly error: string | null;
}

type Listener = () => void;
const listeners = new Set<Listener>();

let current: SessionsStoreState = {
  sessions: [],
  loaded: false,
  loading: false,
  error: null,
};

const notify = (): void => {
  for (const l of listeners) l();
};

const subscribe = (l: Listener): (() => void) => {
  listeners.add(l);
  return (): void => {
    listeners.delete(l);
  };
};

const getSnapshot = (): SessionsStoreState => current;

/** Test/internal hook — reset back to empty state. */
export const __resetSessionsStore = (): void => {
  current = { sessions: [], loaded: false, loading: false, error: null };
  replayCache.clear();
  replayHookState.clear();
  notify();
};

const isSessionSummary = (v: unknown): v is SessionSummary => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.id === "string" &&
    (o.started_at_micros === null || typeof o.started_at_micros === "number") &&
    (o.ended_at_micros === null || typeof o.ended_at_micros === "number") &&
    typeof o.event_count === "number" &&
    typeof o.has_recording === "boolean" &&
    (o.recording_size_bytes === null ||
      typeof o.recording_size_bytes === "number")
  );
};

const isListResult = (
  v: unknown,
): v is { sessions: ReadonlyArray<SessionSummary> } => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return Array.isArray(o.sessions) && o.sessions.every(isSessionSummary);
};

const isReplayResult = (v: unknown): v is ReplayResult => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.event_count === "number" &&
    o.state !== null &&
    typeof o.state === "object"
  );
};

/**
 * Fetch the sessions list from the engine and cache it. Subsequent
 * calls dedupe via the `loading` flag — fine for the History panel
 * which only mounts on tab click. Force-reload by calling with
 * `{ force: true }`.
 */
export const fetchSessions = async (
  client: JsonRpcWS,
  opts: { force?: boolean } = {},
): Promise<SessionsStoreState> => {
  if (current.loading) return current;
  if (current.loaded && !opts.force) return current;
  current = { ...current, loading: true };
  notify();
  try {
    const result = await client.call<unknown>("engine.list_sessions");
    if (isListResult(result)) {
      current = {
        sessions: result.sessions,
        loaded: true,
        loading: false,
        error: null,
      };
    } else {
      current = {
        sessions: [],
        loaded: true,
        loading: false,
        error: "engine returned an unexpected list_sessions shape",
      };
    }
  } catch (err) {
    current = {
      ...current,
      loading: false,
      loaded: true,
      error: err instanceof Error && err.message ? err.message : "RPC error",
    };
  }
  notify();
  return current;
};

/**
 * React hook returning the cached sessions snapshot. Pass a live
 * `JsonRpcWS`; on first mount it kicks off `engine.list_sessions`.
 */
// Track clients we've already wired an onOpen subscriber for.
// `WeakSet` so a client GC'd by the runtime doesn't pin its
// subscription. Module-level (NOT hook-local) so the subscription
// survives mount/unmount of the History panel — without this, a
// reconnect that fires while the panel is hidden gets lost, and
// the next mount sees `loaded=true` + skips the refetch (Codex
// #224 R1 P1 finding).
const sessionsClientsSubscribed: WeakSet<JsonRpcWS> = new WeakSet();

const ensureSessionsOpenSubscribed = (client: JsonRpcWS): void => {
  if (sessionsClientsSubscribed.has(client)) return;
  const ow = (
    client as { onOpen?: (cb: () => void) => () => void }
  ).onOpen;
  if (typeof ow !== "function") return;
  sessionsClientsSubscribed.add(client);
  ow.call(client, (): void => {
    void fetchSessions(client, { force: true });
  });
};

export const useSessions = (client: JsonRpcWS): SessionsStoreState => {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  useEffect((): void => {
    // Wire the persistent reconnect subscription once per client.
    // Cheap idempotent — early-returns on the second call for the
    // same client.
    ensureSessionsOpenSubscribed(client);
    if (!snapshot.loaded && !snapshot.loading) {
      void fetchSessions(client);
    }
  }, [client, snapshot.loaded, snapshot.loading]);
  return snapshot;
};

// ----------------------------------------------------------------
// Replay cache — small LRU per `session_id`.
// ----------------------------------------------------------------

const REPLAY_CACHE_LIMIT = 8;
const replayCache = new Map<string, ReplayResult>();

/**
 * Fetch the replayed `EngineState` for one session, hitting the LRU
 * cache first. Cache key = `session_id`; cache eviction = oldest entry
 * on overflow (Map iteration order is insertion order).
 *
 * Returns `null` on RPC failure — the History panel renders an error
 * banner above the snapshot panel.
 */
export const fetchReplay = async (
  client: JsonRpcWS,
  sessionId: string,
): Promise<ReplayResult | null> => {
  const cached = replayCache.get(sessionId);
  if (cached) {
    // Refresh LRU position.
    replayCache.delete(sessionId);
    replayCache.set(sessionId, cached);
    return cached;
  }
  try {
    const result = await client.call<unknown>("engine.replay_session", {
      session_id: sessionId,
    });
    if (!isReplayResult(result)) return null;
    replayCache.set(sessionId, result);
    while (replayCache.size > REPLAY_CACHE_LIMIT) {
      const first = replayCache.keys().next().value;
      if (typeof first === "string") replayCache.delete(first);
      else break;
    }
    return result;
  } catch {
    return null;
  }
};

/**
 * Hook variant returning the replayed snapshot for a single session,
 * or `null` while fetching. Tracks loading state via `useState`-style
 * `useSyncExternalStore` on a per-instance store the hook owns; we
 * avoid pulling in a heavy state lib for one-shot fetches.
 */
export interface UseReplayState {
  readonly result: ReplayResult | null;
  readonly loading: boolean;
  readonly error: string | null;
}

const replayHookSubs = new Map<string, Set<Listener>>();
const replayHookState = new Map<string, UseReplayState>();

/** Stable initial state so `useSyncExternalStore` does not loop on
 * repeated identity-distinct `{}` literals. */
const INITIAL_REPLAY_STATE: UseReplayState = Object.freeze({
  result: null,
  loading: false,
  error: null,
});

const getReplayState = (sessionId: string): UseReplayState =>
  replayHookState.get(sessionId) ?? INITIAL_REPLAY_STATE;

const notifyReplay = (sessionId: string): void => {
  const subs = replayHookSubs.get(sessionId);
  if (!subs) return;
  for (const l of subs) l();
};

const subscribeReplay = (
  sessionId: string,
  l: Listener,
): (() => void) => {
  let set = replayHookSubs.get(sessionId);
  if (!set) {
    set = new Set();
    replayHookSubs.set(sessionId, set);
  }
  set.add(l);
  return (): void => {
    set?.delete(l);
    if (set && set.size === 0) {
      replayHookSubs.delete(sessionId);
    }
  };
};

/**
 * React hook that fetches + caches a replay for `sessionId` and
 * surfaces the load state. Pass `null` for `sessionId` when no session
 * is selected — the hook then returns `{ result: null, loading: false,
 * error: null }`.
 */
export const useReplay = (
  client: JsonRpcWS,
  sessionId: string | null,
): UseReplayState => {
  // Stabilise the subscribe + snapshot callbacks per session id —
  // useSyncExternalStore would otherwise see a new identity on every
  // render and re-subscribe in a tight loop.
  const key = sessionId ?? "__none__";
  const subscribeFn = useCallback(
    (l: Listener): (() => void) => subscribeReplay(key, l),
    [key],
  );
  const snapshotFn = useCallback((): UseReplayState => getReplayState(key), [
    key,
  ]);
  const snapshot = useSyncExternalStore(subscribeFn, snapshotFn, snapshotFn);
  useEffect((): void => {
    if (!sessionId) return;
    const existing = replayHookState.get(sessionId);
    const cached = replayCache.get(sessionId);
    if (cached) {
      // Only publish + notify if the cached value isn't already current.
      if (!existing || existing.result !== cached) {
        replayHookState.set(sessionId, {
          result: cached,
          loading: false,
          error: null,
        });
        notifyReplay(sessionId);
      }
      return;
    }
    // If a fetch is already in flight or finished, do not re-issue —
    // the hook state already reflects the right shape.
    if (existing && (existing.loading || existing.result || existing.error)) {
      return;
    }
    replayHookState.set(sessionId, {
      result: null,
      loading: true,
      error: null,
    });
    notifyReplay(sessionId);
    void (async (): Promise<void> => {
      const r = await fetchReplay(client, sessionId);
      replayHookState.set(sessionId, {
        result: r,
        loading: false,
        error: r === null ? "replay failed" : null,
      });
      notifyReplay(sessionId);
    })();
  }, [client, sessionId]);
  return snapshot;
};

/**
 * Format a micros-since-epoch timestamp as a user-friendly local
 * date/time. Falls back to "—" on null/invalid input so the UI never
 * shows literal "null".
 */
export const formatTimestampMicros = (ts: number | null): string => {
  if (ts === null || !Number.isFinite(ts)) return "—";
  const ms = ts / 1000;
  if (!Number.isFinite(ms)) return "—";
  try {
    const d = new Date(ms);
    if (Number.isNaN(d.getTime())) return "—";
    return d.toLocaleString();
  } catch {
    return "—";
  }
};

/** Format a duration between two micros-since-epoch timestamps. */
export const formatDurationMicros = (
  start: number | null,
  end: number | null,
): string => {
  if (start === null || end === null || end < start) return "—";
  const seconds = Math.round((end - start) / 1_000_000);
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  if (m < 60) return `${m}m ${s.toString().padStart(2, "0")}s`;
  const h = Math.floor(m / 60);
  const mm = m % 60;
  return `${h}h ${mm.toString().padStart(2, "0")}m`;
};

/**
 * Wire shape for `engine.export_session` results — mirrors the Rust
 * `ExportSummary` in `engine/src/recording/export.rs`.
 */
export interface ExportSummary {
  readonly input_duration_s: number;
  readonly output_duration_s: number;
  readonly trimmed_head_s: number;
  readonly trimmed_tail_s: number;
  readonly chapter_count: number;
  readonly output_path: string;
  readonly chapters_path: string;
}

const isExportSummary = (v: unknown): v is ExportSummary => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.input_duration_s === "number" &&
    typeof o.output_duration_s === "number" &&
    typeof o.trimmed_head_s === "number" &&
    typeof o.trimmed_tail_s === "number" &&
    typeof o.chapter_count === "number" &&
    typeof o.output_path === "string" &&
    typeof o.chapters_path === "string"
  );
};

/**
 * Call `engine.export_session` for the given session id. Returns the
 * `ExportSummary` on success or a string error message on failure
 * (so the caller can show a toast directly without re-throwing).
 *
 * `outputPath` is optional; when omitted the engine writes to the
 * platform-default downloads dir (`~/Downloads/<session_id>.wav` on
 * macOS/Linux).
 */
export const exportSession = async (
  client: JsonRpcWS,
  sessionId: string,
  outputPath?: string,
): Promise<ExportSummary | { error: string }> => {
  try {
    const params: Record<string, string> = { session_id: sessionId };
    if (outputPath) params.output_path = outputPath;
    const result = await client.call<unknown>("engine.export_session", params);
    if (isExportSummary(result)) return result;
    return { error: "engine returned an unexpected export_session shape" };
  } catch (err) {
    return {
      error: err instanceof Error && err.message ? err.message : "export failed",
    };
  }
};

/** Pretty-print byte counts (1.0 KB, 2.4 MB, etc.) for the recording column. */
export const formatBytes = (bytes: number | null): string => {
  if (bytes === null || !Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(1)} KB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  const gb = mb / 1024;
  return `${gb.toFixed(2)} GB`;
};
