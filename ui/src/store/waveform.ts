// Waveform peak-pairs store + React hook.
//
// Fetches base64-encoded min/max peak pairs from the copilot via
// `library.get_waveform`, decodes them once, and caches the resulting
// `Int8Array` by track id. Subsequent reads of the same track id are
// O(1) — both the track-load drag-drop case and the prefetch-on-mount
// case share the same cache.
//
// Wire shape:
//   { track_id: "...", peaks_b64: "<base64>" | null }
// Bytes layout (after base64 decode):
//   [ min_0, max_0, min_1, max_1, ..., min_{N-1}, max_{N-1} ]
// Each value is an `i8` in [-128, 127] mapping audio [-1.0, 1.0].
//
// Why not Float32Array: the wire shape is i8 to keep the payload tiny
// (2000 buckets = 4 KB). The canvas draw path only needs a uniform
// integer-to-pixel mapping which Int8Array gives us for free.

import { useEffect, useState } from "react";
import type { JsonRpcWS } from "../ws/client";

/**
 * LRU cap on the in-memory peaks cache. 50 tracks × 4 KB ≈ 200 KB —
 * negligible memory, and well past the size of any plausible Deck-
 * cycling session (the user opens at most a handful of tracks per
 * minute; 50 covers an hour of continuous browsing).
 */
const CACHE_MAX_ENTRIES = 50;

/**
 * Insertion-ordered cache. `Map.set` on an existing key updates the
 * value without moving its insertion order, so `noteAccess` deletes
 * + re-inserts to bump recency. Falls back to LRU eviction once
 * `CACHE_MAX_ENTRIES` is exceeded.
 */
const cache = new Map<string, Int8Array>();

/** Active in-flight fetch promises, keyed by track id — dedupes
 * concurrent `useWaveform` calls across the two decks if they happen
 * to land on the same track id (rare but trivial to support). */
const inflight = new Map<string, Promise<Int8Array | null>>();

const noteAccess = (trackId: string, value: Int8Array): void => {
  // Re-insert so the most-recent entry is at the back of the iteration
  // order. The first key becomes the LRU candidate for eviction.
  if (cache.has(trackId)) cache.delete(trackId);
  cache.set(trackId, value);
  while (cache.size > CACHE_MAX_ENTRIES) {
    const oldest = cache.keys().next().value;
    if (oldest === undefined) break;
    cache.delete(oldest);
  }
};

/** Decode a base64 string into an Int8Array. ``atob`` is available
 * in browsers, jsdom, and Node 16+ globals — no Node-specific
 * fallback required. */
const decodeBase64 = (b64: string): Int8Array => {
  const decoded = atob(b64);
  const out = new Int8Array(decoded.length);
  for (let i = 0; i < decoded.length; i++) {
    // `charCodeAt` is in [0, 255]; reinterpret as signed via bit math.
    const u = decoded.charCodeAt(i);
    out[i] = u > 127 ? u - 256 : u;
  }
  return out;
};

interface WaveformRpcResponse {
  readonly track_id: string;
  readonly peaks_b64: string | null;
}

const isWaveformResponse = (v: unknown): v is WaveformRpcResponse => {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.track_id === "string" &&
    (o.peaks_b64 === null || typeof o.peaks_b64 === "string")
  );
};

/** Test/internal helper — drop the cache. */
export const __resetWaveformCache = (): void => {
  cache.clear();
  inflight.clear();
};

/** Test/internal helper — seed a track's peaks without an RPC roundtrip. */
export const __setCachedPeaks = (trackId: string, peaks: Int8Array): void => {
  noteAccess(trackId, peaks);
};

/** Test/internal helper — read the current cache size. */
export const __cacheSize = (): number => cache.size;

/**
 * Fetch peaks for a track. Cached subsequent calls return the same
 * `Int8Array` reference (cheap to `useMemo`). Returns `null` when
 * the copilot reports `peaks_b64: null` (un-analyzed) or the RPC
 * fails — callers render the flat-line fallback.
 */
export const fetchWaveform = async (
  client: JsonRpcWS,
  trackId: string,
): Promise<Int8Array | null> => {
  const cached = cache.get(trackId);
  if (cached) {
    noteAccess(trackId, cached); // bump recency
    return cached;
  }
  const pending = inflight.get(trackId);
  if (pending) return pending;

  const p = (async (): Promise<Int8Array | null> => {
    try {
      const result = await client.call<unknown>("library.get_waveform", {
        track_id: trackId,
      });
      if (!isWaveformResponse(result)) return null;
      if (result.peaks_b64 === null) return null;
      const peaks = decodeBase64(result.peaks_b64);
      noteAccess(trackId, peaks);
      return peaks;
    } catch {
      return null;
    } finally {
      inflight.delete(trackId);
    }
  })();
  inflight.set(trackId, p);
  return p;
};

/**
 * React hook — fetches peaks for ``trackId`` (or returns ``null``
 * while pending / when ``trackId`` is null). Re-renders once when
 * the peaks arrive. Cached across decks so reloading the same track
 * is instantaneous.
 */
export const useWaveform = (
  client: JsonRpcWS,
  trackId: string | null,
): Int8Array | null => {
  const [peaks, setPeaks] = useState<Int8Array | null>(
    (): Int8Array | null => {
      if (!trackId) return null;
      return cache.get(trackId) ?? null;
    },
  );

  useEffect((): (() => void) | void => {
    if (!trackId) {
      setPeaks(null);
      return;
    }
    const cached = cache.get(trackId);
    if (cached) {
      setPeaks(cached);
      return;
    }
    setPeaks(null);
    let cancelled = false;
    void fetchWaveform(client, trackId).then((result): void => {
      if (cancelled) return;
      setPeaks(result);
    });
    return (): void => {
      cancelled = true;
    };
  }, [client, trackId]);

  return peaks;
};
