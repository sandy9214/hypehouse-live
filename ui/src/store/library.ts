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
}

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
        tracks: result.tracks,
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
    if (isSearchResult(result)) return result.tracks;
    return [];
  } catch {
    // Search errors are quiet — the UI just shows "no matches".
    return [];
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
