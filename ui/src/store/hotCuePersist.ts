// Hot-cue persistence bridge.
//
// Owns the contract that the engine *executes* a `HotCueSet` event
// (mutating `Deck::hot_cues` in-memory), and the co-pilot library
// *persists* the updated array to its SQLite catalog. The bridge sits
// in the UI for v0.1 (the engine doesn't talk to the co-pilot
// directly — see PR #40), debouncing rapid pad-presses into one DB
// write per ~500ms idle window.
//
// Why module-scope rather than React state:
//   * Multiple UI surfaces emit `HotCueSet` — the deck pads, MIDI
//     translator, future copilot triggers. A module-scope queue means
//     all of them flow through one debounce timer per (track, deck).
//   * The engine's `state_changed` mirror already holds the
//     authoritative 8-slot array post-event. We pull from there at
//     flush time rather than building a separate accumulator.
//
// Backwards compat: if the loaded track id is unknown (e.g. a
// non-library DeckLoad), `recordHotCueSet` is a no-op. Library tracks
// always have an id so the common path persists; ad-hoc loads
// don't pollute the DB with phantom rows.

import type { JsonRpcWS } from "../ws/client";
import { setHotCues } from "./library";

/** ms idle window before flushing a queued write. */
export const DEFAULT_DEBOUNCE_MS = 500;

interface PendingFlush {
  trackId: string;
  /** Latest snapshot of the deck's 8-slot hot-cue grid. */
  cues: ReadonlyArray<number | null>;
  /** Debounce timer handle. Cleared on coalesce / cancel. */
  timer: ReturnType<typeof setTimeout> | null;
}

// One pending flush per deck id. v0.1 has two decks (A, B); this
// scales trivially if a future PR introduces N decks.
const pending = new Map<string, PendingFlush>();

/**
 * Module-scope mapping from deck id to the currently-loaded library
 * track id. Populated by `noteLoadedTrack` (called from TrackRow load
 * button + Deck drop handler) and read at flush time. Decoupled from
 * the engine store so we can adjust hot-cue persistence without
 * re-shaping the engine state mirror.
 */
const loadedTrack = new Map<string, string>();

/** Record which library track is currently loaded on a deck. */
export const noteLoadedTrack = (deckId: string, trackId: string): void => {
  loadedTrack.set(deckId, trackId);
  // Loading a new track invalidates any queued write for the deck —
  // those cues belong to the old track and would corrupt the new
  // track's row if we flushed them now.
  const queued = pending.get(deckId);
  if (queued && queued.timer !== null) {
    clearTimeout(queued.timer);
  }
  pending.delete(deckId);
};

/** Read which track id is currently loaded on a deck — testing helper. */
export const __getLoadedTrack = (deckId: string): string | undefined =>
  loadedTrack.get(deckId);

/** Clear all queued writes + loaded-track memory. Test helper. */
export const __resetHotCuePersist = (): void => {
  for (const p of pending.values()) {
    if (p.timer !== null) clearTimeout(p.timer);
  }
  pending.clear();
  loadedTrack.clear();
};

/**
 * Queue a `library.set_hot_cues` write for `deckId`'s currently-loaded
 * track. Coalesces with any in-flight queued write — the most recent
 * `cues` array wins. After `debounceMs` of idle, flushes to the RPC
 * client.
 *
 * No-op if the deck doesn't have a known library track (e.g. an
 * ad-hoc DeckLoad outside the library flow). The engine still applies
 * the in-memory `HotCueSet` either way; persistence is best-effort.
 */
export const recordHotCueSet = (
  client: JsonRpcWS,
  deckId: string,
  cues: ReadonlyArray<number | null>,
  debounceMs: number = DEFAULT_DEBOUNCE_MS,
): void => {
  const trackId = loadedTrack.get(deckId);
  if (!trackId) return;

  // Coalesce: cancel the prior timer (we'll reschedule with the
  // freshest snapshot below).
  const prior = pending.get(deckId);
  if (prior && prior.timer !== null) {
    clearTimeout(prior.timer);
  }

  const entry: PendingFlush = { trackId, cues, timer: null };
  entry.timer = setTimeout((): void => {
    entry.timer = null;
    pending.delete(deckId);
    // Fire-and-forget — `setHotCues` already swallows transport errors.
    void setHotCues(client, entry.trackId, entry.cues);
  }, debounceMs);
  pending.set(deckId, entry);
};

/**
 * Flush any pending write for `deckId` immediately. Used on deck
 * unload / tab close so a half-debounced cue array doesn't get lost.
 * Returns the pending track id when a flush actually fired, or
 * `null` when nothing was queued.
 */
export const flushHotCuePersist = (
  client: JsonRpcWS,
  deckId: string,
): string | null => {
  const queued = pending.get(deckId);
  if (!queued) return null;
  if (queued.timer !== null) clearTimeout(queued.timer);
  pending.delete(deckId);
  void setHotCues(client, queued.trackId, queued.cues);
  return queued.trackId;
};
