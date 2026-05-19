// TrackRow.tsx — single library list entry.
//
// Renders the row content (title, id, BPM, key, duration) + per-deck
// "→ A" / "→ B" load buttons. Click submits a ``DeckLoad`` event
// over JsonRpcWS; payload shape is the externally-tagged
// `EventKind::DeckLoad` from engine/src/state.rs (track + bpm +
// beat_grid_anchor_ms + downbeats_ms + 8-slot hot_cues).
//
// Hover preview (PR ui-library-waveform-hover): mouseenter debounces
// 200 ms then fetches peaks via the waveform store and renders a
// compact `Waveform`. Mobile uses touchstart + 500 ms long-press. A
// per-trackId timestamp skips re-fetch within 30 s.

import { useCallback, useEffect, useRef, useState } from "react";
import type {
  CSSProperties,
  DragEvent as ReactDragEvent,
  JSX,
  TouchEvent as ReactTouchEvent,
} from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId } from "../store/engine";
import type { LibraryTrack } from "../store/library";
import { noteLoadedTrack } from "../store/hotCuePersist";
import { fetchWaveform } from "../store/waveform";
import { Waveform } from "./Waveform";
import { enqueueTrack } from "../store/playlist";

export interface TrackRowProps {
  track: LibraryTrack;
  client: JsonRpcWS;
  // Optional handler so callers can wire row activation to a higher-
  // level effect (focus, scroll-into-view) without re-implementing.
  onLoaded?: (deck: DeckId, track: LibraryTrack) => void;
  /** Hover debounce ms (mouseenter -> fetch). Tests override to 0. */
  hoverDebounceMs?: number;
  /** Long-press hold ms (touchstart -> fetch). Tests override. */
  longPressMs?: number;
  /** Recency gate for repeat hovers — re-hover within this window
   * skips the RPC and re-uses any cached peaks. */
  rehoverCacheMs?: number;
}

const HOVER_DEBOUNCE_MS_DEFAULT = 200;
const LONG_PRESS_MS_DEFAULT = 500;
const REHOVER_CACHE_MS_DEFAULT = 30_000;
const PREVIEW_HEIGHT = 32;
const PREVIEW_WIDTH = 480;

/** Per-trackId timestamp of last hover-triggered fetch. Module-level
 * so an unmount + remount (search-filter churn) keeps the gate. */
const lastHoverAt = new Map<string, number>();

/** Test/internal helper — drop the recency map. */
export const __resetTrackRowHoverCache = (): void => { lastHoverAt.clear(); };

const rowWrapStyle: CSSProperties = { borderBottom: "1px solid #222" };
const rowStyle: CSSProperties = {
  display: "grid",
  // 7-column grid; trailing column widened to fit the deck-load
  // buttons (→ A / → B) plus the "→ Queue" enqueue button added in
  // the playlist-queue PR. Keep in sync with the matching
  // ``columnsRowStyle`` header in `Library.tsx`.
  gridTemplateColumns: "2fr 1fr 64px 56px 72px 156px",
  alignItems: "center",
  gap: 8,
  padding: "4px 8px",
  color: "#ddd",
  fontFamily: "monospace",
  fontSize: 12,
};
const previewStyle: CSSProperties = { padding: "2px 8px 4px 8px", background: "#080808" };
const cellStyle: CSSProperties = {
  overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap",
};
const btnStyle: CSSProperties = {
  background: "#222", color: "#fff", border: "1px solid #444",
  borderRadius: 4, padding: "2px 6px", cursor: "pointer",
  fontFamily: "monospace", fontSize: 12,
};

const fmtDuration = (s: number): string => {
  const total = Math.max(0, Math.round(s));
  const m = Math.floor(total / 60);
  const sec = total % 60;
  return `${m}:${sec.toString().padStart(2, "0")}`;
};

const titleOf = (track: LibraryTrack): string => {
  const stem = track.path.split("/").pop() ?? track.id;
  const dot = stem.lastIndexOf(".");
  return dot > 0 ? stem.slice(0, dot) : stem;
};

const submitDeckLoad = (
  client: JsonRpcWS,
  deck: DeckId,
  track: LibraryTrack,
): Promise<unknown> => {
  return client.call("submit_event", {
    DeckLoad: {
      deck,
      track: { id: track.id, path: track.path },
      bpm: track.bpm,
      beat_grid_anchor_ms: track.beat_grid_anchor_ms,
      downbeats_ms: track.downbeats_ms,
      hot_cues: Array.from(track.hot_cues),
    },
  });
};

export const TrackRow = ({
  track,
  client,
  onLoaded,
  hoverDebounceMs = HOVER_DEBOUNCE_MS_DEFAULT,
  longPressMs = LONG_PRESS_MS_DEFAULT,
  rehoverCacheMs = REHOVER_CACHE_MS_DEFAULT,
}: TrackRowProps): JSX.Element => {
  const [previewPeaks, setPreviewPeaks] = useState<Int8Array | null>(null);
  const [previewOpen, setPreviewOpen] = useState<boolean>(false);
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cancelledRef = useRef<boolean>(false);

  const clearTimer = useCallback((): void => {
    if (hoverTimer.current !== null) {
      clearTimeout(hoverTimer.current);
      hoverTimer.current = null;
    }
  }, []);

  useEffect((): (() => void) => {
    return (): void => {
      cancelledRef.current = true;
      clearTimer();
    };
  }, [clearTimer]);

  const openPreview = useCallback((): void => {
    setPreviewOpen(true);
    const now = Date.now();
    const last = lastHoverAt.get(track.id);
    // Within the recency window: store cache will hit, no fresh RPC
    // is issued (fetchWaveform short-circuits on cached peaks).
    // Outside: bump the recency timestamp and let fetch run.
    if (last === undefined || now - last >= rehoverCacheMs) {
      lastHoverAt.set(track.id, now);
    }
    void fetchWaveform(client, track.id).then((peaks): void => {
      if (cancelledRef.current) return;
      setPreviewPeaks(peaks);
    });
  }, [client, track.id, rehoverCacheMs]);

  const closePreview = useCallback((): void => {
    clearTimer();
    setPreviewOpen(false);
  }, [clearTimer]);

  const handleMouseEnter = useCallback((): void => {
    clearTimer();
    hoverTimer.current = setTimeout(openPreview, hoverDebounceMs);
  }, [clearTimer, hoverDebounceMs, openPreview]);

  const handleTouchStart = useCallback(
    (e: ReactTouchEvent<HTMLDivElement>): void => {
      // Single-finger long-press only; multi-touch reserved for future
      // pinch-to-zoom on the loaded-deck waveform.
      if (e.touches.length !== 1) return;
      clearTimer();
      hoverTimer.current = setTimeout(openPreview, longPressMs);
    },
    [clearTimer, longPressMs, openPreview],
  );

  const handleLoad = (deck: DeckId): void => {
    noteLoadedTrack(deck, track.id);
    void submitDeckLoad(client, deck, track)
      .then((): void => { onLoaded?.(deck, track); })
      // v0.1 swallows errors — toast layer lands in a later PR.
      .catch((): void => {});
  };

  const handleDragStart = (e: ReactDragEvent<HTMLDivElement>): void => {
    e.dataTransfer.effectAllowed = "copy";
    e.dataTransfer.setData(
      "application/x-hypehouse-track",
      JSON.stringify(track),
    );
  };

  return (
    <div data-testid={`track-row-wrap-${track.id}`} style={rowWrapStyle}>
      <div
        data-testid={`track-row-${track.id}`}
        role="listitem"
        draggable
        onDragStart={handleDragStart}
        onMouseEnter={handleMouseEnter}
        onMouseLeave={closePreview}
        onTouchStart={handleTouchStart}
        onTouchEnd={closePreview}
        onTouchCancel={closePreview}
        style={rowStyle}
      >
        <span style={cellStyle} title={titleOf(track)}>
          {titleOf(track)}
        </span>
        <span style={cellStyle} title={track.id}>
          {track.id}
        </span>
        <span style={cellStyle}>{track.bpm.toFixed(1)}</span>
        <span style={cellStyle}>{track.camelot_key}</span>
        <span style={cellStyle}>{fmtDuration(track.duration_s)}</span>
        <span style={{ ...cellStyle, display: "flex", gap: 4 }}>
          <button
            type="button"
            aria-label={`Load ${track.id} on deck A`}
            data-testid={`load-${track.id}-A`}
            style={btnStyle}
            onClick={(): void => handleLoad("A")}
          >
            → A
          </button>
          <button
            type="button"
            aria-label={`Load ${track.id} on deck B`}
            data-testid={`load-${track.id}-B`}
            style={btnStyle}
            onClick={(): void => handleLoad("B")}
          >
            → B
          </button>
          <button
            type="button"
            aria-label={`Enqueue ${track.id} to playlist`}
            data-testid={`enqueue-${track.id}`}
            style={btnStyle}
            onClick={(): void => {
              void enqueueTrack(client, track.id);
            }}
          >
            → Queue
          </button>
        </span>
      </div>
      {previewOpen ? (
        <div
          data-testid={`track-row-preview-${track.id}`}
          style={previewStyle}
        >
          <Waveform
            peaks={previewPeaks}
            height={PREVIEW_HEIGHT}
            width={PREVIEW_WIDTH}
            compactMode
          />
        </div>
      ) : null}
    </div>
  );
};
