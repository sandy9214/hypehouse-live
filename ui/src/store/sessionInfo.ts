// Engine session-info store.
//
// Single fetch on first hook subscriber via `engine.session_info`.
// Pure read — result reflects the env / build at the moment the
// engine started. The UI re-fetches only on explicit refresh (e.g.
// engine reconnect); we don't subscribe to state_changed because
// the payload doesn't carry session-static fields.

import { useEffect, useSyncExternalStore } from "react";
import type { JsonRpcWS } from "../ws/client";

export interface SessionFeatures {
  midi_clock_in: boolean;
  midi_clock_out: boolean;
  ableton_link: boolean;
  sentry_telemetry: boolean;
  recording_enabled: boolean;
  rate_limit_disabled: boolean;
  shared_ci_runner: boolean;
}

export interface SessionInfo {
  version: string;
  output_device_substring: string;
  features: SessionFeatures;
}

export const DEFAULT_SESSION_INFO: SessionInfo = {
  version: "",
  output_device_substring: "",
  features: {
    midi_clock_in: false,
    midi_clock_out: false,
    ableton_link: false,
    sentry_telemetry: false,
    recording_enabled: true,
    rate_limit_disabled: false,
    shared_ci_runner: false,
  },
};

const isSessionInfo = (v: unknown): v is SessionInfo => {
  if (!v || typeof v !== "object") return false;
  const obj = v as Record<string, unknown>;
  if (typeof obj.version !== "string") return false;
  if (typeof obj.output_device_substring !== "string") return false;
  const feats = obj.features;
  if (!feats || typeof feats !== "object") return false;
  for (const k of Object.keys(DEFAULT_SESSION_INFO.features) as Array<
    keyof SessionFeatures
  >) {
    if (typeof (feats as Record<string, unknown>)[k] !== "boolean") return false;
  }
  return true;
};

type Listener = () => void;
const listeners: Set<Listener> = new Set();
let current: SessionInfo = DEFAULT_SESSION_INFO;
let fetchInFlight = false;
let fetchedOnce = false;

const notify = (): void => {
  for (const l of listeners) l();
};
const subscribe = (l: Listener): (() => void) => {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
};
const getSnapshot = (): SessionInfo => current;

/** Test seam. */
export const __setSessionInfo = (next: SessionInfo): void => {
  current = next;
  fetchedOnce = true;
  notify();
};

/** Reset — test only. */
export const __resetSessionInfo = (): void => {
  current = DEFAULT_SESSION_INFO;
  fetchedOnce = false;
  fetchInFlight = false;
  notify();
};

export const fetchSessionInfo = async (
  client: JsonRpcWS,
): Promise<SessionInfo> => {
  if (fetchedOnce) return current;
  if (fetchInFlight) return current;
  fetchInFlight = true;
  try {
    const result = await client.call<unknown>("engine.session_info");
    if (isSessionInfo(result)) {
      __setSessionInfo(result);
    } else {
      __setSessionInfo(DEFAULT_SESSION_INFO);
    }
  } catch {
    __setSessionInfo(DEFAULT_SESSION_INFO);
  } finally {
    fetchInFlight = false;
  }
  return current;
};

/** React hook — kicks off the fetch on first subscriber. */
export const useSessionInfo = (client: JsonRpcWS): SessionInfo => {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  useEffect(() => {
    void fetchSessionInfo(client);
  }, [client]);
  return snapshot;
};


// ---- Cloud library sync status (#102 follow-up) -------------------

export interface SyncStatus {
  pending_push_count: number;
  library_track_count: number;
  /** Wall-clock micros — `0` before the daemon's first successful tick. */
  last_pull_micros: number;
  last_push_micros: number;
  last_pull_fetched: number;
  last_pull_applied: number;
  last_push_pushed: number;
  last_tick_error: string;
  /** Wall-clock micros of the next scheduled tick. `0` before the
   * first tick. With backoff active, drifts out exponentially under
   * sustained failures so the UI countdown reflects the real wait. */
  next_sync_micros: number;
}

const DEFAULT_SYNC_STATUS: SyncStatus = {
  pending_push_count: 0,
  library_track_count: 0,
  last_pull_micros: 0,
  last_push_micros: 0,
  last_pull_fetched: 0,
  last_pull_applied: 0,
  last_push_pushed: 0,
  last_tick_error: "",
  next_sync_micros: 0,
};

const isSyncStatus = (v: unknown): v is SyncStatus => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  // Pre-stats-fold engines (#155-era) ship just the two counts;
  // accept that shape and fill defaults so the UI doesn't break
  // during an in-place upgrade. The full shape arrives once #157+
  // copilot is deployed.
  return (
    typeof o.pending_push_count === "number" &&
    typeof o.library_track_count === "number"
  );
};

const normaliseSyncStatus = (v: unknown): SyncStatus => {
  if (!isSyncStatus(v)) return DEFAULT_SYNC_STATUS;
  const o = v as unknown as Record<string, unknown>;
  const num = (k: string): number =>
    typeof o[k] === "number" ? (o[k] as number) : 0;
  const str = (k: string): string =>
    typeof o[k] === "string" ? (o[k] as string) : "";
  return {
    pending_push_count: num("pending_push_count"),
    library_track_count: num("library_track_count"),
    last_pull_micros: num("last_pull_micros"),
    last_push_micros: num("last_push_micros"),
    last_pull_fetched: num("last_pull_fetched"),
    last_pull_applied: num("last_pull_applied"),
    last_push_pushed: num("last_push_pushed"),
    last_tick_error: str("last_tick_error"),
    next_sync_micros: num("next_sync_micros"),
  };
};

/**
 * Format a "next sync in Xs" countdown from the daemon's planned
 * `next_sync_micros` timestamp. `0` → empty string (caller hides the
 * row entirely). Negative deltas (overdue ticks — daemon thread is
 * lagging) render as "due" rather than a meaningless negative count.
 */
export const formatCountdownMicros = (
  nextMicros: number,
  nowMs: number = Date.now(),
): string => {
  if (!Number.isFinite(nextMicros) || nextMicros <= 0) return "";
  const deltaMs = nextMicros / 1000 - nowMs;
  if (deltaMs <= 0) return "due";
  const s = Math.ceil(deltaMs / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const sRem = s % 60;
  return sRem === 0 ? `${m}m` : `${m}m ${sRem}s`;
};

type SyncStatusListener = () => void;
const syncStatusListeners: Set<SyncStatusListener> = new Set();
let syncStatus: SyncStatus = DEFAULT_SYNC_STATUS;

const notifySync = (): void => {
  for (const l of syncStatusListeners) l();
};
const subscribeSync = (l: SyncStatusListener): (() => void) => {
  syncStatusListeners.add(l);
  return () => {
    syncStatusListeners.delete(l);
  };
};
const getSyncSnapshot = (): SyncStatus => syncStatus;

export const __setSyncStatus = (next: SyncStatus): void => {
  syncStatus = next;
  notifySync();
};

export const __resetSyncStatus = (): void => {
  syncStatus = DEFAULT_SYNC_STATUS;
  notifySync();
};

/**
 * Re-fetch cloud sync status. Cheap RPC (one count + one len query
 * on a tiny table); safe to call on a refresh poll.
 */
export const fetchSyncStatus = async (
  client: JsonRpcWS,
): Promise<SyncStatus> => {
  try {
    const result = await client.call<unknown>("library.sync_status");
    __setSyncStatus(normaliseSyncStatus(result));
  } catch {
    __setSyncStatus(DEFAULT_SYNC_STATUS);
  }
  return syncStatus;
};

/**
 * Operator-driven "queue all" — fires the
 * `library.requeue_all_pending` RPC. Used after a pre-cloud-sync
 * upgrade (or any time the operator wants to re-seed the cloud
 * from the local library). Returns the post-call total queued
 * count so the caller can render a toast.
 */
export const requeueAllPending = async (
  client: JsonRpcWS,
): Promise<number> => {
  const result = await client.call<unknown>(
    "library.requeue_all_pending",
  );
  if (
    result &&
    typeof result === "object" &&
    typeof (result as { queued?: unknown }).queued === "number"
  ) {
    const queued = (result as { queued: number }).queued;
    // The sync_status RPC fires on the next polling tick, but
    // forcing it here keeps the AboutPanel pending count in step
    // with the operator's just-clicked action.
    void fetchSyncStatus(client);
    void fetchPendingPushIds(client);
    return queued;
  }
  return 0;
};

/**
 * Operator-driven force sync. Fires the `library.sync_now` RPC,
 * folds the post-tick status into the store, and bubbles up the
 * resolved status so the caller can show a toast. Errors (e.g.
 * cloud-sync not configured) propagate; the store keeps the prior
 * snapshot in that case so the badge doesn't flicker to "never".
 */
export const syncNow = async (client: JsonRpcWS): Promise<SyncStatus> => {
  const result = await client.call<unknown>("library.sync_now");
  const next = normaliseSyncStatus(result);
  __setSyncStatus(next);
  return next;
};

/**
 * Format a wall-clock micros timestamp as a relative "X ago" string.
 * `0` → "never". Otherwise "Xs ago" / "Xm ago" / "Xh ago" / "Xd ago"
 * depending on the magnitude — same convention as GitHub timestamps.
 */
export const formatRelativeMicros = (
  micros: number,
  nowMs: number = Date.now(),
): string => {
  if (!Number.isFinite(micros) || micros <= 0) return "never";
  const deltaMs = nowMs - micros / 1000;
  if (deltaMs < 0) return "just now"; // clock skew between engine + UI
  const s = Math.floor(deltaMs / 1000);
  if (s < 5) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  return `${d}d ago`;
};

// ---- Pending-push id set (#164 follow-up) -------------------------
//
// Separate store from `SyncStatus`. The status RPC returns counts +
// last-tick stats, but rendering a per-row indicator needs the
// actual ID set. We poll on the same cadence to stay cheap; the
// PostgREST + library calls are independent so they overlap fine.

type PendingPushSet = ReadonlySet<string>;

const EMPTY_PENDING: PendingPushSet = new Set<string>();
let pendingPushSet: PendingPushSet = EMPTY_PENDING;
const pendingPushListeners: Set<() => void> = new Set();

const notifyPendingPush = (): void => {
  for (const l of pendingPushListeners) l();
};
const subscribePendingPush = (l: () => void): (() => void) => {
  pendingPushListeners.add(l);
  return () => {
    pendingPushListeners.delete(l);
  };
};
const getPendingPushSnapshot = (): PendingPushSet => pendingPushSet;

/** Test seam. */
export const __setPendingPushIds = (ids: Iterable<string>): void => {
  pendingPushSet = new Set(ids);
  notifyPendingPush();
};

/** Reset — test only. */
export const __resetPendingPushIds = (): void => {
  pendingPushSet = EMPTY_PENDING;
  notifyPendingPush();
};

/**
 * Fetch the current pending-push id set via the
 * `library.list_pending_push` RPC and stash it in the store.
 * Falls back to the empty set on transport error.
 */
export const fetchPendingPushIds = async (
  client: JsonRpcWS,
): Promise<PendingPushSet> => {
  try {
    const result = await client.call<unknown>("library.list_pending_push");
    if (
      result &&
      typeof result === "object" &&
      Array.isArray((result as { ids?: unknown }).ids)
    ) {
      const ids = (result as { ids: unknown[] }).ids.filter(
        (id): id is string => typeof id === "string",
      );
      __setPendingPushIds(ids);
    } else {
      __setPendingPushIds([]);
    }
  } catch {
    __setPendingPushIds([]);
  }
  return pendingPushSet;
};

/** Hook — fetches once + polls every `refreshMs`. */
export const usePendingPushIds = (
  client: JsonRpcWS,
  refreshMs = 5_000,
): PendingPushSet => {
  const snapshot = useSyncExternalStore(
    subscribePendingPush,
    getPendingPushSnapshot,
    getPendingPushSnapshot,
  );
  useEffect(() => {
    void fetchPendingPushIds(client);
    const id = window.setInterval(() => {
      void fetchPendingPushIds(client);
    }, refreshMs);
    return () => window.clearInterval(id);
  }, [client, refreshMs]);
  return snapshot;
};

// ---- Stems status (#194 follow-up) -------------------------------
//
// Aggregate counts by demucs stems-status bucket. Backed by the
// `library.stems_status` RPC. Polls on the same cadence as the
// other status hooks so the AboutPanel "Stems: N ready" row stays
// fresh without per-track round trips.

export interface StemsStatus {
  readonly ready: number;
  readonly pending: number;
  readonly failed: number;
  readonly none: number;
}

const DEFAULT_STEMS_STATUS: StemsStatus = {
  ready: 0,
  pending: 0,
  failed: 0,
  none: 0,
};

const isStemsStatus = (v: unknown): v is StemsStatus => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.ready === "number" &&
    typeof o.pending === "number" &&
    typeof o.failed === "number" &&
    typeof o.none === "number"
  );
};

type StemsStatusListener = () => void;
const stemsStatusListeners: Set<StemsStatusListener> = new Set();
let stemsStatus: StemsStatus = DEFAULT_STEMS_STATUS;

const notifyStems = (): void => {
  for (const l of stemsStatusListeners) l();
};
const subscribeStems = (l: StemsStatusListener): (() => void) => {
  stemsStatusListeners.add(l);
  return () => {
    stemsStatusListeners.delete(l);
  };
};
const getStemsSnapshot = (): StemsStatus => stemsStatus;

export const __setStemsStatus = (next: StemsStatus): void => {
  stemsStatus = next;
  notifyStems();
};

export const __resetStemsStatus = (): void => {
  stemsStatus = DEFAULT_STEMS_STATUS;
  notifyStems();
};

export const fetchStemsStatus = async (
  client: JsonRpcWS,
): Promise<StemsStatus> => {
  try {
    const result = await client.call<unknown>("library.stems_status");
    if (isStemsStatus(result)) {
      __setStemsStatus(result);
    } else {
      __setStemsStatus(DEFAULT_STEMS_STATUS);
    }
  } catch {
    __setStemsStatus(DEFAULT_STEMS_STATUS);
  }
  return stemsStatus;
};

export const useStemsStatus = (
  client: JsonRpcWS,
  refreshMs = 15_000,
): StemsStatus => {
  // Stems counts change at human-import cadence, not by the second —
  // 15s default is enough.
  const snapshot = useSyncExternalStore(
    subscribeStems,
    getStemsSnapshot,
    getStemsSnapshot,
  );
  useEffect(() => {
    void fetchStemsStatus(client);
    const id = window.setInterval(() => {
      void fetchStemsStatus(client);
    }, refreshMs);
    return () => window.clearInterval(id);
  }, [client, refreshMs]);
  return snapshot;
};

/** Hook — fetches once on mount; polls every `refreshMs` while hooked. */
export const useSyncStatus = (
  client: JsonRpcWS,
  refreshMs = 5_000,
): SyncStatus => {
  const snapshot = useSyncExternalStore(
    subscribeSync,
    getSyncSnapshot,
    getSyncSnapshot,
  );
  useEffect(() => {
    void fetchSyncStatus(client);
    const id = window.setInterval(() => {
      void fetchSyncStatus(client);
    }, refreshMs);
    return () => window.clearInterval(id);
  }, [client, refreshMs]);
  return snapshot;
};
