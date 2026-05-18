// Effects manifest store.
//
// One fetch on mount via `engine.list_effects`; the result is the
// static catalogue of built-in effects (id, name, param descriptors)
// the engine ships with. UI components read it via
// `useEffectsManifest()` and render dropdowns + param controls
// without re-asking the engine on every render.
//
// The manifest is process-static — caching for the whole UI session
// is safe and the engine handler is also cheap (pure CPU, no I/O),
// so a single fetch on first hook subscriber is enough.

import { useEffect, useSyncExternalStore } from "react";
import type { JsonRpcWS } from "../ws/client";

/** One param descriptor — mirrors `audio::effects::ParamDescriptor`. */
export interface EffectParamDescriptor {
  name: string;
  min: number;
  max: number;
  default: number;
}

/** One effect entry in the manifest. */
export interface EffectManifestEntry {
  id: number;
  name: string;
  params: ReadonlyArray<EffectParamDescriptor>;
}

export type EffectManifest = ReadonlyArray<EffectManifestEntry>;

type ListenerSet = Set<() => void>;

const listeners: ListenerSet = new Set();
let manifest: EffectManifest = [];
let fetchInFlight = false;
let fetchedOnce = false;

const notify = (): void => {
  for (const l of listeners) l();
};

const subscribe = (l: () => void): (() => void) => {
  listeners.add(l);
  return (): void => {
    listeners.delete(l);
  };
};

const getSnapshot = (): EffectManifest => manifest;

/**
 * Apply a freshly-fetched manifest. Public so tests / mock clients
 * can seed without going through the RPC layer.
 */
export const __setEffectsManifest = (next: EffectManifest): void => {
  manifest = next;
  fetchedOnce = true;
  notify();
};

/** Reset manifest state — test only. */
export const __resetEffectsManifest = (): void => {
  manifest = [];
  fetchedOnce = false;
  fetchInFlight = false;
  notify();
};

/** Wire shape of `engine.list_effects` result. */
interface ListEffectsResult {
  effects: ReadonlyArray<EffectManifestEntry>;
}

const isListEffectsResult = (v: unknown): v is ListEffectsResult => {
  if (!v || typeof v !== "object") return false;
  const obj = v as { effects?: unknown };
  return Array.isArray(obj.effects);
};

/**
 * Fetch manifest from the engine if it hasn't been loaded yet. Multiple
 * concurrent calls are deduped via the `fetchInFlight` flag.
 */
export const fetchEffectsManifest = async (
  client: JsonRpcWS,
): Promise<EffectManifest> => {
  if (fetchedOnce) return manifest;
  if (fetchInFlight) return manifest;
  fetchInFlight = true;
  try {
    const result = await client.call<unknown>("engine.list_effects");
    if (isListEffectsResult(result)) {
      __setEffectsManifest(result.effects);
    } else {
      // Bad shape — keep manifest empty; UI shows "no effects".
      __setEffectsManifest([]);
    }
  } catch {
    // Network / RPC error: leave manifest empty. The UI handles `[]`
    // by showing all slots as "None" only.
    __setEffectsManifest([]);
  } finally {
    fetchInFlight = false;
  }
  return manifest;
};

/**
 * React hook returning the cached effects manifest. Pass a live
 * `JsonRpcWS` client; on first mount the hook kicks off a fetch.
 * Until that resolves, returns `[]` (empty manifest = "no effects
 * available" UX path).
 */
export const useEffectsManifest = (client: JsonRpcWS): EffectManifest => {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  useEffect((): void => {
    // Fire-and-forget; the store update inside resolves notifies us.
    void fetchEffectsManifest(client);
  }, [client]);
  return snapshot;
};
