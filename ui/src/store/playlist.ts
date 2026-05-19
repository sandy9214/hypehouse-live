// Playlist queue store.
//
// Mirrors the wire surface from `copilot/playlist_rpc.py`:
//   * `playlist.list`     -> { entries: PlaylistEntry[] }
//   * `playlist.enqueue`  -> { entry:   PlaylistEntry    }
//   * `playlist.reorder`  -> { entries: PlaylistEntry[] }
//   * `playlist.remove`   -> { entries: PlaylistEntry[] }
//   * `playlist.clear`    -> { ok: true }
//
// Module-singleton state shared between the PlaylistQueue panel and any
// future "drag from library" affordance, like `store/library.ts`. Uses
// `useSyncExternalStore` so React 18 sees consistent snapshots across
// re-renders.

import { useEffect, useSyncExternalStore } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { LibraryTrack } from "./library";

/**
 * One entry in the queue. `track` is `null` when the track row was
 * removed from the library after the entry was enqueued — the UI
 * surfaces those with a "missing" badge instead of dropping them, so
 * the operator can explicitly remove them.
 */
export interface PlaylistEntry {
  readonly track_id: string;
  readonly position: number;
  readonly added_at: string;
  readonly track: LibraryTrack | null;
}

export interface PlaylistListResult {
  readonly entries: ReadonlyArray<PlaylistEntry>;
}

interface PlaylistStoreState {
  entries: ReadonlyArray<PlaylistEntry>;
  loaded: boolean;
  loading: boolean;
  // Last RPC error message — null on success / first load. The panel
  // shows a small inline banner so an empty list with an error is
  // distinct from an empty list "queue is empty" state.
  error: string | null;
}

type Listener = () => void;
const listeners = new Set<Listener>();

let current: PlaylistStoreState = {
  entries: [],
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

const getSnapshot = (): PlaylistStoreState => current;

/** Test/internal hook — reset back to empty state. */
export const __resetPlaylistStore = (): void => {
  current = { entries: [], loaded: false, loading: false, error: null };
  notify();
};

/** Test/internal hook — preload state without an RPC round-trip. */
export const __setPlaylistEntries = (
  entries: ReadonlyArray<PlaylistEntry>,
): void => {
  current = { entries, loaded: true, loading: false, error: null };
  notify();
};

const isPlaylistEntry = (v: unknown): v is PlaylistEntry => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.track_id === "string" &&
    typeof o.position === "number" &&
    typeof o.added_at === "string"
  );
};

const isListResult = (v: unknown): v is PlaylistListResult => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return Array.isArray(o.entries) && o.entries.every(isPlaylistEntry);
};

const setFromList = (entries: ReadonlyArray<PlaylistEntry>): void => {
  current = { entries, loaded: true, loading: false, error: null };
  notify();
};

const setError = (msg: string): void => {
  current = {
    ...current,
    loading: false,
    loaded: true,
    error: msg || "RPC error",
  };
  notify();
};

const errMsg = (err: unknown): string => {
  return err instanceof Error && err.message ? err.message : "RPC error";
};

/**
 * Fetch the current queue from `playlist.list`. Dedupes via `loading`
 * so a fast re-mount doesn't fire two parallel RPCs. Subsequent calls
 * after a mutation should use the more specific returned `entries`
 * from the mutation RPC instead.
 */
export const fetchPlaylist = async (
  client: JsonRpcWS,
): Promise<PlaylistStoreState> => {
  if (current.loading) return current;
  current = { ...current, loading: true };
  notify();
  try {
    const result = await client.call<unknown>("playlist.list", {});
    if (isListResult(result)) {
      setFromList(result.entries);
    } else {
      setFromList([]);
    }
  } catch (err) {
    setError(errMsg(err));
  }
  return current;
};

/**
 * Append `trackId` to the tail of the queue. Resolves with the
 * post-mutation snapshot or `null` on RPC failure (UI keeps cached
 * state + surfaces the error).
 */
export const enqueueTrack = async (
  client: JsonRpcWS,
  trackId: string,
): Promise<ReadonlyArray<PlaylistEntry> | null> => {
  try {
    await client.call<unknown>("playlist.enqueue", { track_id: trackId });
    // Re-fetch the full queue rather than splicing locally — the wire
    // shape from `playlist.enqueue` is a single entry, not the whole
    // ordered list, and a remote reorder could land between our
    // append + the next read. Cheap (one round-trip; queues are tiny).
    const result = await client.call<unknown>("playlist.list", {});
    if (isListResult(result)) {
      setFromList(result.entries);
      return result.entries;
    }
    return null;
  } catch (err) {
    setError(errMsg(err));
    return null;
  }
};

/**
 * Move `trackId` to the 0-indexed `newPosition`. Server-side clamps
 * out-of-range positions to the nearest edge.
 */
export const reorderTrack = async (
  client: JsonRpcWS,
  trackId: string,
  newPosition: number,
): Promise<ReadonlyArray<PlaylistEntry> | null> => {
  try {
    const result = await client.call<unknown>("playlist.reorder", {
      track_id: trackId,
      new_position: newPosition,
    });
    if (isListResult(result)) {
      setFromList(result.entries);
      return result.entries;
    }
    return null;
  } catch (err) {
    setError(errMsg(err));
    return null;
  }
};

export const removeTrack = async (
  client: JsonRpcWS,
  trackId: string,
): Promise<ReadonlyArray<PlaylistEntry> | null> => {
  try {
    const result = await client.call<unknown>("playlist.remove", {
      track_id: trackId,
    });
    if (isListResult(result)) {
      setFromList(result.entries);
      return result.entries;
    }
    return null;
  } catch (err) {
    setError(errMsg(err));
    return null;
  }
};

export const clearPlaylist = async (client: JsonRpcWS): Promise<boolean> => {
  try {
    await client.call<unknown>("playlist.clear", {});
    setFromList([]);
    return true;
  } catch (err) {
    setError(errMsg(err));
    return false;
  }
};

/**
 * React hook returning the cached queue + auto-loading on first mount.
 * Pass a live `JsonRpcWS`; the first mount fires `playlist.list`.
 */
export const usePlaylist = (client: JsonRpcWS): PlaylistStoreState => {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  useEffect((): void => {
    if (!snapshot.loaded && !snapshot.loading) {
      void fetchPlaylist(client);
    }
  }, [client, snapshot.loaded, snapshot.loading]);
  return snapshot;
};
