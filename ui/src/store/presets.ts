// Preset snapshots store — thin client over the copilot's
// `presets.*` JSON-RPC namespace.
//
// One module-level cache holds the list of preset summaries (id +
// name + created_at). Saves / deletes optimistically update the cache
// then re-fetch on success so the UI is reactive without round-trip
// latency. The full preset body is fetched on demand by the load
// flow — keeping the cached list tiny lets us scale to hundreds of
// presets without bloating the in-memory mirror.
//
// Wire shapes mirror `copilot.preset_rpc` exactly. See
// `copilot/preset_rpc.py` for the authoritative surface.

import { useEffect, useSyncExternalStore } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { CrossfaderCurve } from "./engine";

/** Lightweight preset row — `presets.list` returns just this shape. */
export interface PresetSummary {
  readonly id: number;
  readonly name: string;
  readonly created_at: string;
}

/** One effect slot inside a preset (mirrors the engine's `EffectSlot`). */
export interface PresetEffectSlot {
  readonly effect_id: number;
  readonly params: Readonly<Record<string, number>>;
  readonly wet_dry: number;
  readonly enabled: boolean;
}

/** Per-deck snapshot inside a preset. */
export interface PresetDeckState {
  readonly effects: ReadonlyArray<PresetEffectSlot>;
  readonly eq_low_db: number;
  readonly eq_mid_db: number;
  readonly eq_high_db: number;
  readonly pitch_semitones: number;
  readonly tempo_ratio: number;
}

/** Full preset shape — `presets.load` / `presets.save` return this. */
export interface Preset {
  readonly id: number;
  readonly name: string;
  readonly created_at: string;
  readonly deck_a: PresetDeckState;
  readonly deck_b: PresetDeckState;
  readonly crossfader_curve: CrossfaderCurve;
}

interface PresetsStoreState {
  presets: ReadonlyArray<PresetSummary>;
  loaded: boolean;
  loading: boolean;
  error: string | null;
}

type Listener = () => void;
const listeners = new Set<Listener>();

let current: PresetsStoreState = {
  presets: [],
  loaded: false,
  loading: false,
  error: null,
};

// Monotonic generation counter. Every mutation (`savePreset` success,
// `deletePreset` success) bumps this BEFORE updating `current`, so an
// in-flight `presets.list` whose response arrives later discards
// itself instead of replacing the cache with a stale list — i.e. it
// avoids "just-saved preset disappears" / "just-deleted preset
// re-appears" races (Codex #231 R1 finding).
let cacheGeneration = 0;
const bumpGeneration = (): number => {
  cacheGeneration += 1;
  return cacheGeneration;
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

const getSnapshot = (): PresetsStoreState => current;

/** Test/internal hook — reset to empty state. */
export const __resetPresetsStore = (): void => {
  current = { presets: [], loaded: false, loading: false, error: null };
  cacheGeneration = 0;
  notify();
};

/** Test/internal hook — direct snapshot access without the React hook. */
export const __getPresetsSnapshot = (): PresetsStoreState => current;

/** Test/internal seed — drop a list of summaries into the store. */
export const __setPresets = (
  presets: ReadonlyArray<PresetSummary>,
): void => {
  current = { presets, loaded: true, loading: false, error: null };
  notify();
};

const isSummary = (v: unknown): v is PresetSummary => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.id === "number" &&
    typeof o.name === "string" &&
    typeof o.created_at === "string"
  );
};

const isPreset = (v: unknown): v is Preset => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.id === "number" &&
    typeof o.name === "string" &&
    typeof o.created_at === "string" &&
    typeof o.crossfader_curve === "string" &&
    o.deck_a !== undefined &&
    o.deck_b !== undefined
  );
};

/**
 * Fetch the full list from `presets.list`. Dedup'd via `loading` flag.
 * Returns the post-call state so callers (the panel mount effect) can
 * react inline without a second hook tick.
 *
 * `opts.force = true` is reserved for the reconnect-refetch path —
 * see `refetchPresets`. It bypasses the `loaded` check (which lets
 * cached state stay) but still respects the `loading` dedupe so a
 * reconnect that fires mid-flight doesn't double-call.
 */
export const fetchPresets = async (
  client: JsonRpcWS,
  opts: { force?: boolean } = {},
): Promise<PresetsStoreState> => {
  if (current.loading) return current;
  if (!opts.force && current.loaded) return current;
  // Snapshot the generation at the start of the RPC. If a save /
  // delete mutation runs before our response lands, the generations
  // diverge and we drop the now-stale list rather than clobbering
  // the optimistic update. (Codex #231 R1 P1.)
  const fetchGen = cacheGeneration;
  current = { ...current, loading: true };
  notify();
  try {
    const raw = await client.call<unknown>("presets.list", {});
    if (cacheGeneration !== fetchGen) {
      // A mutation completed mid-flight; the cache it produced is
      // truthier than our list snapshot. Drop loading flag without
      // touching `presets` / `loaded`.
      current = { ...current, loading: false };
      notify();
      return current;
    }
    if (
      raw &&
      typeof raw === "object" &&
      "presets" in raw &&
      Array.isArray((raw as { presets: unknown }).presets)
    ) {
      const list = (raw as { presets: unknown[] }).presets.filter(isSummary);
      current = {
        presets: list,
        loaded: true,
        loading: false,
        error: null,
      };
    } else {
      current = {
        presets: [],
        loaded: true,
        loading: false,
        error: "presets service returned an unexpected shape",
      };
    }
  } catch (err) {
    if (cacheGeneration !== fetchGen) {
      // Mutation landed during a failing fetch — keep the new cache;
      // don't surface the stale error.
      current = { ...current, loading: false };
      notify();
      return current;
    }
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
 * Force-refresh the cached preset list, bypassing the `loaded` guard.
 * Used by the WS reconnect subscriber: server-side presets may have
 * been added / deleted / renamed via another client (cloud sync, a
 * second window) during the gap, so the cache is stale-by-default
 * after a reconnect.
 */
export const refetchPresets = (
  client: JsonRpcWS,
): Promise<PresetsStoreState> => fetchPresets(client, { force: true });

/**
 * Save a new preset. On success the cache is refreshed in-place so the
 * panel re-renders with the new row at the top. On a duplicate-name
 * error (`-32602` from the copilot) we surface the error message in
 * the store so the panel can show it inline.
 */
export const savePreset = async (
  client: JsonRpcWS,
  params: {
    name: string;
    deck_a: PresetDeckState;
    deck_b: PresetDeckState;
    crossfader_curve: CrossfaderCurve;
  },
): Promise<Preset | null> => {
  try {
    const result = await client.call<unknown>("presets.save", params);
    if (
      result &&
      typeof result === "object" &&
      "preset" in result &&
      isPreset((result as { preset: unknown }).preset)
    ) {
      const saved = (result as { preset: Preset }).preset;
      const summary: PresetSummary = {
        id: saved.id,
        name: saved.name,
        created_at: saved.created_at,
      };
      // Optimistic prepend — the next fetchPresets re-orders by recency.
      // Bump generation so any in-flight `presets.list` discards its
      // stale response on return instead of erasing this new row.
      bumpGeneration();
      current = {
        ...current,
        presets: [summary, ...current.presets.filter((p) => p.id !== saved.id)],
        loaded: true,
        error: null,
      };
      notify();
      return saved;
    }
    current = { ...current, error: "presets.save returned unexpected shape" };
    notify();
    return null;
  } catch (err) {
    current = {
      ...current,
      error: err instanceof Error && err.message ? err.message : "RPC error",
    };
    notify();
    return null;
  }
};

/** Fetch one preset's full body. */
export const loadPreset = async (
  client: JsonRpcWS,
  id: number,
): Promise<Preset | null> => {
  try {
    const result = await client.call<unknown>("presets.load", { id });
    if (
      result &&
      typeof result === "object" &&
      "preset" in result &&
      isPreset((result as { preset: unknown }).preset)
    ) {
      return (result as { preset: Preset }).preset;
    }
    return null;
  } catch {
    return null;
  }
};

/**
 * Delete a preset by id. On success removes the row from the cache.
 * Returns true on success (including idempotent "already gone") and
 * false on RPC error so the UI can show a toast.
 */
export const deletePreset = async (
  client: JsonRpcWS,
  id: number,
): Promise<boolean> => {
  try {
    await client.call<unknown>("presets.delete", { id });
    // Bump generation so any in-flight presets.list discards on
    // return instead of resurrecting the deleted row.
    bumpGeneration();
    current = {
      ...current,
      presets: current.presets.filter((p) => p.id !== id),
    };
    notify();
    return true;
  } catch {
    return false;
  }
};

// Module-level WeakSet of clients we've already wired an onOpen
// subscriber for. Mirrors the pattern in `sessions.ts` (PR #224 R1
// fix) so a reconnect that fires while the presets panel is hidden
// still triggers a fresh `presets.list` against the new socket. The
// WeakSet doesn't pin the client — runtime GC reclaims both
// together.
const presetsClientsSubscribed: WeakSet<JsonRpcWS> = new WeakSet();

const ensurePresetsOpenSubscribed = (client: JsonRpcWS): void => {
  if (presetsClientsSubscribed.has(client)) return;
  const ow = (
    client as { onOpen?: (cb: () => void) => () => void }
  ).onOpen;
  if (typeof ow !== "function") return;
  presetsClientsSubscribed.add(client);
  ow.call(client, (): void => {
    void refetchPresets(client);
  });
};

/**
 * React hook returning the cached preset list. Auto-fetches on first
 * mount when the cache is empty and not yet in flight. Idempotently
 * wires a module-level WS-reconnect subscriber so the cache
 * self-refreshes after a socket bounce even while the panel is
 * unmounted.
 */
export const usePresets = (client: JsonRpcWS): PresetsStoreState => {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  useEffect((): void => {
    ensurePresetsOpenSubscribed(client);
    if (!snapshot.loaded && !snapshot.loading) {
      void fetchPresets(client);
    }
  }, [client, snapshot.loaded, snapshot.loading]);
  return snapshot;
};
