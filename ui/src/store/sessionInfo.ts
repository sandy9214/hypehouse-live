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
}

const DEFAULT_SYNC_STATUS: SyncStatus = {
  pending_push_count: 0,
  library_track_count: 0,
};

const isSyncStatus = (v: unknown): v is SyncStatus => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.pending_push_count === "number" &&
    typeof o.library_track_count === "number"
  );
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
    if (isSyncStatus(result)) {
      __setSyncStatus(result);
    } else {
      __setSyncStatus(DEFAULT_SYNC_STATUS);
    }
  } catch {
    __setSyncStatus(DEFAULT_SYNC_STATUS);
  }
  return syncStatus;
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
