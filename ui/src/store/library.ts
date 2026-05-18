// Track library store.
//
// Mirrors the shape returned by the copilot's ``library.list_tracks`` /
// ``library.search_tracks`` JSON-RPC methods (see
// `copilot/library_rpc.py`). One global cache keyed on the
// (query, limit) pair so the Library panel and any later "browse from
// the deck" affordance share fetched data.
//
// The store deliberately keeps zero React state internally — it uses
// `useSyncExternalStore` like `store/engine.ts` so React 18 sees
// consistent snapshots across re-renders.

import { useEffect, useSyncExternalStore } from "react";
import type { JsonRpcWS } from "../ws/client";

/**
 * One row in the library — wire shape from
 * `copilot.library_rpc.track_ref_to_wire`. ``id`` matches the engine's
 * ``state::TrackRef.id`` so the row can be passed straight into a
 * ``DeckLoad`` event.
 */
export interface LibraryTrack {
  readonly id: string;
  readonly path: string;
  readonly bpm: number;
  readonly camelot_key: string;
  readonly energy: number;
  readonly duration_s: number;
  readonly beat_grid_anchor_ms: number;
  readonly beat_period_ms: number;
  readonly downbeats_ms: ReadonlyArray<number>;
  // 8-slot hot-cue grid — `number` (ms position) per set slot, `null`
  // per empty slot. Mirrors the engine's `Deck::hot_cues: [Option<u64>; 8]`
  // and the library DB's `hot_cues_json` column. Optional on the wire
  // for backwards compat with copilots that haven't migrated yet —
  // `loadHotCues` below normalises a missing/short array to 8 nulls.
  readonly hot_cues: ReadonlyArray<number | null>;
}

/**
 * Number of hot-cue slots per deck — keep in sync with the engine's
 * `Deck::hot_cues` array length (8) and the copilot's
 * `HOT_CUE_SLOTS` constant.
 */
export const HOT_CUE_SLOTS = 8;

/**
 * Build a fresh empty hot-cue array. Helper exists so test fixtures
 * and the library backwards-compat path don't sprinkle inline
 * `[null,null,null,null,null,null,null,null]` literals.
 */
export const emptyHotCues = (): ReadonlyArray<number | null> =>
  Array.from({ length: HOT_CUE_SLOTS }, (): number | null => null);

export interface LibraryListResult {
  readonly tracks: ReadonlyArray<LibraryTrack>;
  readonly total: number;
  readonly limit: number;
  readonly offset: number;
}

export interface LibrarySearchResult {
  readonly tracks: ReadonlyArray<LibraryTrack>;
  readonly query: string;
  readonly limit: number;
}

interface LibraryStoreState {
  tracks: ReadonlyArray<LibraryTrack>;
  total: number;
  loaded: boolean;
  loading: boolean;
  // Last error message — null on success / first-load. UI surfaces this
  // as a small banner so an empty list with an error is distinct from
  // an empty list "no tracks yet" state.
  error: string | null;
}

type Listener = () => void;
const listeners = new Set<Listener>();

let current: LibraryStoreState = {
  tracks: [],
  total: 0,
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

const getSnapshot = (): LibraryStoreState => current;

/** Test/internal hook — reset back to empty state. */
export const __resetLibraryStore = (): void => {
  current = {
    tracks: [],
    total: 0,
    loaded: false,
    loading: false,
    error: null,
  };
  notify();
};

/**
 * Seed the store with a result — used by tests + by the search-tracks
 * action which writes the matching subset into the cache.
 */
export const __setLibraryTracks = (
  tracks: ReadonlyArray<LibraryTrack>,
  total: number,
): void => {
  current = {
    tracks,
    total,
    loaded: true,
    loading: false,
    error: null,
  };
  notify();
};

/**
 * Normalize a wire `hot_cues` value to a fresh 8-slot array. Accepts:
 *   * a well-formed array → preserves `number`/`null` shape;
 *   * an array shorter than 8 → right-pads with `null`;
 *   * `undefined` / `null` / wrong type → returns 8 nulls.
 * Keeps the UI defensively stable against pre-PR copilots that don't
 * emit the field yet.
 */
const normalizeHotCues = (
  raw: unknown,
): ReadonlyArray<number | null> => {
  const out: Array<number | null> = Array.from(
    { length: HOT_CUE_SLOTS },
    (): number | null => null,
  );
  if (!Array.isArray(raw)) return out;
  for (let i = 0; i < HOT_CUE_SLOTS && i < raw.length; i++) {
    const v = raw[i] as unknown;
    if (typeof v === "number" && Number.isFinite(v) && v >= 0) {
      out[i] = v;
    }
  }
  return out;
};

const isLibraryTrack = (v: unknown): v is LibraryTrack => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.id === "string" &&
    typeof o.path === "string" &&
    typeof o.bpm === "number" &&
    typeof o.camelot_key === "string" &&
    typeof o.energy === "number" &&
    typeof o.duration_s === "number"
  );
};

/**
 * Apply hot-cue normalization to a wire-shape track. Returns a fresh
 * object with `hot_cues` guaranteed to be an 8-slot array — callers
 * never have to branch on the legacy "field missing" path.
 */
const hydrateTrack = (raw: LibraryTrack): LibraryTrack => ({
  ...raw,
  hot_cues: normalizeHotCues((raw as { hot_cues?: unknown }).hot_cues),
});

const isListResult = (v: unknown): v is LibraryListResult => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    Array.isArray(o.tracks) &&
    o.tracks.every(isLibraryTrack) &&
    typeof o.total === "number"
  );
};

const isSearchResult = (v: unknown): v is LibrarySearchResult => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return Array.isArray(o.tracks) && o.tracks.every(isLibraryTrack);
};

/**
 * Fetch one page from `library.list_tracks` and cache it. Subsequent
 * calls with the same args are deduped via the ``loading`` flag.
 */
export const fetchLibrary = async (
  client: JsonRpcWS,
  opts: { limit?: number; offset?: number } = {},
): Promise<LibraryStoreState> => {
  if (current.loading) return current;
  current = { ...current, loading: true };
  notify();
  const limit = opts.limit ?? 100;
  const offset = opts.offset ?? 0;
  try {
    const result = await client.call<unknown>("library.list_tracks", {
      limit,
      offset,
    });
    if (isListResult(result)) {
      current = {
        tracks: result.tracks.map(hydrateTrack),
        total: result.total,
        loaded: true,
        loading: false,
        error: null,
      };
    } else {
      current = {
        tracks: [],
        total: 0,
        loaded: true,
        loading: false,
        error: "library service returned an unexpected shape",
      };
    }
  } catch (err) {
    current = {
      ...current,
      loading: false,
      loaded: true,
      // Empty error message becomes "RPC error" so the banner reads sensibly.
      error: err instanceof Error && err.message ? err.message : "RPC error",
    };
  }
  notify();
  return current;
};

/**
 * Run `library.search_tracks` against a query string and return the
 * matching rows. Does **not** mutate the cached "all tracks" snapshot
 * — search results are presentational. The Library component owns the
 * "what to display" decision.
 */
export const searchLibrary = async (
  client: JsonRpcWS,
  query: string,
  opts: { limit?: number } = {},
): Promise<ReadonlyArray<LibraryTrack>> => {
  const limit = opts.limit ?? 100;
  try {
    const result = await client.call<unknown>("library.search_tracks", {
      query,
      limit,
    });
    if (isSearchResult(result)) return result.tracks.map(hydrateTrack);
    return [];
  } catch {
    // Search errors are quiet — the UI just shows "no matches".
    return [];
  }
};

/**
 * Persist a hot-cue array against a track in the library DB. Returns
 * the updated `LibraryTrack` on success and `null` on RPC failure —
 * callers (the Deck pad handler) treat failure as "best-effort
 * write" and don't block the user's hot-cue UX on it.
 */
export const setHotCues = async (
  client: JsonRpcWS,
  trackId: string,
  hotCues: ReadonlyArray<number | null>,
): Promise<LibraryTrack | null> => {
  try {
    const result = await client.call<unknown>("library.set_hot_cues", {
      track_id: trackId,
      hot_cues: Array.from(hotCues),
    });
    if (
      result &&
      typeof result === "object" &&
      "track" in result &&
      isLibraryTrack((result as { track: unknown }).track)
    ) {
      return hydrateTrack(
        (result as { track: LibraryTrack }).track,
      );
    }
    return null;
  } catch {
    return null;
  }
};

/**
 * React hook returning the cached library snapshot. Pass a live
 * `JsonRpcWS`; on first mount it kicks off `library.list_tracks`.
 */
export const useLibrary = (client: JsonRpcWS): LibraryStoreState => {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  useEffect((): void => {
    if (!snapshot.loaded && !snapshot.loading) {
      void fetchLibrary(client);
    }
  }, [client, snapshot.loaded, snapshot.loading]);
  return snapshot;
};
