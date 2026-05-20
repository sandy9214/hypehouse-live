// Output device picker store.
//
// One fetch on mount via `engine.list_output_devices`. Result is the
// current cpal device list — the host default is flagged. Selection
// (substring → HYPEHOUSE_OUTPUT_DEVICE env var) is persisted to
// localStorage; the engine reads the env var at startup, so a change
// here surfaces only after an engine restart.
//
// Companion to PR #115 (engine endpoint) + issue #111 (livestream
// virtual loopback differentiator).

import { useEffect, useSyncExternalStore } from "react";
import type { JsonRpcWS } from "../ws/client";

const STORAGE_KEY = "hypehouse:outputDeviceSubstring";

export interface OutputDevice {
  name: string;
  is_default: boolean;
}

export type OutputDeviceList = ReadonlyArray<OutputDevice>;

interface ListOutputDevicesResult {
  devices: ReadonlyArray<OutputDevice>;
}

const isListOutputDevicesResult = (v: unknown): v is ListOutputDevicesResult => {
  if (!v || typeof v !== "object") return false;
  const obj = v as { devices?: unknown };
  if (!Array.isArray(obj.devices)) return false;
  return obj.devices.every(
    (d): d is OutputDevice =>
      !!d &&
      typeof d === "object" &&
      typeof (d as OutputDevice).name === "string" &&
      typeof (d as OutputDevice).is_default === "boolean",
  );
};

type ListenerSet = Set<() => void>;
const listeners: ListenerSet = new Set();
let devices: OutputDeviceList = [];
let fetchInFlight = false;
let fetchedOnce = false;

const notify = (): void => {
  for (const l of listeners) l();
};
const subscribe = (l: () => void): (() => void) => {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
};
const getSnapshot = (): OutputDeviceList => devices;

/** Test seam — inject a synthetic device list. */
export const __setOutputDevices = (next: OutputDeviceList): void => {
  devices = next;
  fetchedOnce = true;
  notify();
};

/** Reset store — test only. */
export const __resetOutputDevices = (): void => {
  devices = [];
  fetchedOnce = false;
  fetchInFlight = false;
  notify();
};

/**
 * Force a re-fetch even if the cache is warm. Used by the WS
 * reconnect path (`client.onOpen(...)`) so the dropdown reflects
 * the engine's *current* device list after a restart (e.g. operator
 * plugged in a new USB interface mid-session). Parallels
 * `refetchSessionInfo` from #207.
 */
export const refetchOutputDevices = async (
  client: JsonRpcWS,
): Promise<OutputDeviceList> => {
  fetchedOnce = false;
  return fetchOutputDevices(client);
};

/** Fetch the device list (deduped — safe to call from multiple hooks). */
export const fetchOutputDevices = async (
  client: JsonRpcWS,
): Promise<OutputDeviceList> => {
  if (fetchedOnce) return devices;
  if (fetchInFlight) return devices;
  fetchInFlight = true;
  try {
    const result = await client.call<unknown>("engine.list_output_devices");
    if (isListOutputDevicesResult(result)) {
      __setOutputDevices(result.devices);
    } else {
      __setOutputDevices([]);
    }
  } catch {
    __setOutputDevices([]);
  } finally {
    fetchInFlight = false;
  }
  return devices;
};

/**
 * React hook returning the cached output device list. First mount
 * kicks off a fetch; until that resolves, returns `[]`.
 */
export const useOutputDevices = (client: JsonRpcWS): OutputDeviceList => {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  useEffect(() => {
    void fetchOutputDevices(client);
    // Refresh on every WS reconnect — an engine restart (or a
    // mid-session audio-interface plug/unplug followed by an engine
    // bounce) changes the device list, and the one-shot cache would
    // otherwise hide that until the page reloads. Same `onOpen`
    // hook + typeof-tolerance pattern as `useSessionInfo` (#207).
    const ow = (client as { onOpen?: (cb: () => void) => () => void })
      .onOpen;
    if (typeof ow !== "function") return;
    const unsub = ow.call(client, (): void => {
      void refetchOutputDevices(client);
    });
    return unsub;
  }, [client]);
  return snapshot;
};

/**
 * Read the persisted device-name substring from localStorage. Empty
 * string ≡ "not set" — caller treats as host default.
 */
export const getSelectedDeviceSubstring = (): string => {
  if (typeof window === "undefined") return "";
  try {
    return window.localStorage.getItem(STORAGE_KEY) ?? "";
  } catch {
    return "";
  }
};

/**
 * Persist the device-name substring to localStorage. Pass empty
 * string to clear (revert to host default on next engine restart).
 */
export const setSelectedDeviceSubstring = (substring: string): void => {
  if (typeof window === "undefined") return;
  try {
    const trimmed = substring.trim();
    if (trimmed === "") {
      window.localStorage.removeItem(STORAGE_KEY);
    } else {
      window.localStorage.setItem(STORAGE_KEY, trimmed);
    }
  } catch {
    // localStorage may be disabled (private mode, quota); silent no-op.
  }
};

/**
 * Identify which device in the list matches the persisted substring.
 * Case-insensitive substring match, mirrors the engine's
 * `pick_output_device` logic so the UI displays the same device the
 * engine would pick on next start.
 */
export const matchSelectedDevice = (
  list: OutputDeviceList,
  substring: string,
): OutputDevice | null => {
  const needle = substring.trim().toLowerCase();
  if (needle === "") return null;
  return list.find((d) => d.name.toLowerCase().includes(needle)) ?? null;
};
