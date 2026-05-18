// Stems store — thin wrapper around `library.compute_stems` +
// `library.get_stems` (see `copilot/library_rpc.py`).
//
// Two pieces of UX glue live here:
//
//   1. `requestStems(client, trackId)` — fire-and-forget RPC that kicks
//      the demucs background task. Resolves to the immediate
//      `{status, track_id}` envelope. Re-issuing while a task is
//      running is safe — the copilot de-dupes (returns the same
//      pending envelope).
//
//   2. `useStemStatus(client, trackId)` — React hook that polls
//      `library.get_stems` on a 2-second cadence until the cache
//      reports `"ready"` or `"failed"`, then stops. Returns the
//      ordered `[vocals, drums, bass, other]` paths so the caller can
//      drop them straight into a `DeckLoadStems` event without
//      remembering the canonical ordering.
//
// Why a separate store: stem state is per-track (not per-deck) and
// orthogonal to the event-sourced engine mirror. Co-pilot owns the
// cache lifecycle; the UI just observes + triggers. Keeping it out of
// `engine.ts` matches the same "transient out-of-band signal" rule
// `notifications.ts` follows.

import { useEffect, useRef, useState } from "react";
import type { JsonRpcWS } from "../ws/client";

/**
 * Canonical stem ordering — MUST match `engine/src/state.rs`
 * (`EventKind::DeckLoadStems::stem_paths`) and
 * `copilot/stems.py::STEM_NAMES`. The audio mixer keys per-stem
 * gains off this index, so a re-order here would mute the wrong
 * stem on the wire.
 */
export const STEM_ORDER = ["vocals", "drums", "bass", "other"] as const;
export type StemName = (typeof STEM_ORDER)[number];

/** Wire status union from `library.get_stems`. */
export type StemStatus = "ready" | "pending" | "failed";

/** Public hook-return shape. `paths` is null until the cache is `"ready"`. */
export interface StemStatusSnapshot {
  status: StemStatus | null;
  /** `[vocals, drums, bass, other]` absolute WAV paths, or null. */
  paths: readonly [string, string, string, string] | null;
}

/** Raw shape of `library.get_stems` response (see docs/api/ws-protocol.md). */
interface GetStemsResult {
  track_id?: string;
  status?: StemStatus | null;
  stems?: Partial<Record<StemName, string>> | null;
}

/**
 * Default polling cadence in milliseconds. The hook accepts an override
 * for tests so we don't have to fake the system clock to assert two
 * complete polling intervals.
 */
export const DEFAULT_STEM_POLL_MS = 2000;

const isStemStatus = (v: unknown): v is StemStatus =>
  v === "ready" || v === "pending" || v === "failed";

/** Coerce a wire payload to `StemStatusSnapshot`. Defensive — the
 * copilot's "graceful null" responses (`status: null`) flow through as
 * a `status: null` snapshot rather than crashing the consumer. */
export const parseStemStatus = (raw: unknown): StemStatusSnapshot => {
  if (!raw || typeof raw !== "object") {
    return { status: null, paths: null };
  }
  const result = raw as GetStemsResult;
  const status = isStemStatus(result.status) ? result.status : null;
  if (status !== "ready" || !result.stems) {
    return { status, paths: null };
  }
  const s = result.stems;
  if (
    typeof s.vocals === "string" &&
    typeof s.drums === "string" &&
    typeof s.bass === "string" &&
    typeof s.other === "string"
  ) {
    return {
      status,
      paths: [s.vocals, s.drums, s.bass, s.other] as const,
    };
  }
  // ready + missing fields = treat as failed so the caller can offer
  // a retry rather than spinning forever.
  return { status: "failed", paths: null };
};

/**
 * Kick off stem separation for `trackId`. Returns the initial wire
 * envelope (always `{status: "pending"}` on a fresh request, or a
 * pre-existing pending if a task is already in-flight). Errors are
 * surfaced (e.g. demucs not installed → JSON-RPC `-32000`) so the
 * caller can react.
 */
export const requestStems = async (
  client: JsonRpcWS,
  trackId: string,
): Promise<{ status: StemStatus | null; track_id: string }> => {
  const result = await client.call<unknown>("library.compute_stems", {
    track_id: trackId,
  });
  if (result && typeof result === "object") {
    const r = result as { status?: unknown; track_id?: unknown };
    return {
      status: isStemStatus(r.status) ? r.status : null,
      track_id: typeof r.track_id === "string" ? r.track_id : trackId,
    };
  }
  return { status: null, track_id: trackId };
};

/**
 * Single-shot fetch — used by the hook below + as a public helper for
 * callers that want to peek without subscribing.
 */
export const fetchStemStatus = async (
  client: JsonRpcWS,
  trackId: string,
): Promise<StemStatusSnapshot> => {
  try {
    const result = await client.call<unknown>("library.get_stems", {
      track_id: trackId,
    });
    return parseStemStatus(result);
  } catch {
    // RPC failure — treat as "still pending"; the next poll might
    // succeed (transient network blip). Returning `failed` would lock
    // the UI into a retry button prematurely.
    return { status: "pending", paths: null };
  }
};

/**
 * Hook: subscribe to the stem-cache status for `trackId`. Polls every
 * `pollMs` (default `DEFAULT_STEM_POLL_MS`) until status flips to
 * `"ready"` or `"failed"`, then idles. Re-runs on `trackId` change so
 * a deck swap resets cleanly.
 *
 * Returns `{status: null, paths: null}` when `trackId` is null —
 * common case when no deck has a track loaded yet.
 */
export const useStemStatus = (
  client: JsonRpcWS,
  trackId: string | null,
  pollMs: number = DEFAULT_STEM_POLL_MS,
): StemStatusSnapshot => {
  const [snapshot, setSnapshot] = useState<StemStatusSnapshot>({
    status: null,
    paths: null,
  });
  // Track whether the component is still mounted so a late RPC
  // resolution doesn't `setState` after unmount (React warns in
  // strict mode).
  const aliveRef = useRef<boolean>(true);

  useEffect((): (() => void) => {
    aliveRef.current = true;
    // Reset state on trackId change so a stale "ready" doesn't leak
    // across a deck swap.
    setSnapshot({ status: null, paths: null });
    if (trackId === null) {
      return (): void => {
        aliveRef.current = false;
      };
    }

    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const tick = async (): Promise<void> => {
      if (stopped) return;
      const next = await fetchStemStatus(client, trackId);
      if (stopped || !aliveRef.current) return;
      setSnapshot(next);
      // Stop polling once we have a terminal status. The caller will
      // explicitly re-kick via `requestStems` on retry, which sets the
      // cache back to "pending" and the next mount picks it up.
      if (next.status === "ready" || next.status === "failed") {
        return;
      }
      timer = setTimeout((): void => {
        void tick();
      }, pollMs);
    };
    void tick();

    return (): void => {
      stopped = true;
      aliveRef.current = false;
      if (timer !== null) clearTimeout(timer);
    };
  }, [client, trackId, pollMs]);

  return snapshot;
};
