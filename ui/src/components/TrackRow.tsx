// TrackRow.tsx — single library list entry.
//
// Renders the row content (title, artist, BPM, key, duration) plus
// per-deck "→ A" / "→ B" load buttons. Clicking a button submits a
// ``DeckLoad`` event to the engine over the existing JsonRpcWS.
//
// DeckLoad wire shape lives in `engine/src/state.rs` —
// externally-tagged enum, so the payload is
// ``{ DeckLoad: { deck, track, bpm, beat_grid_anchor_ms,
// downbeats_ms, hot_cues } }``. The ``hot_cues`` field was added in
// the hot-cue persistence PR — it carries the library's saved 8-slot
// cue array so a track always loads with the cues it was last saved
// with (engine reducer copies it directly onto ``Deck::hot_cues``).

import type { CSSProperties, DragEvent as ReactDragEvent, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId } from "../store/engine";
import type { LibraryTrack } from "../store/library";
import { noteLoadedTrack } from "../store/hotCuePersist";

export interface TrackRowProps {
  track: LibraryTrack;
  client: JsonRpcWS;
  // Optional click-handler hooks let parent components (the Deck
  // empty-state hint) wire row activation to a higher-level effect
  // (focus, scroll into view) without re-implementing the button row.
  onLoaded?: (deck: DeckId, track: LibraryTrack) => void;
}

const rowStyle: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "2fr 1fr 64px 56px 72px 96px",
  alignItems: "center",
  gap: 8,
  padding: "4px 8px",
  borderBottom: "1px solid #222",
  color: "#ddd",
  fontFamily: "monospace",
  fontSize: 12,
};

const cellStyle: CSSProperties = {
  overflow: "hidden",
  textOverflow: "ellipsis",
  whiteSpace: "nowrap",
};

const btnStyle: CSSProperties = {
  background: "#222",
  color: "#fff",
  border: "1px solid #444",
  borderRadius: 4,
  padding: "2px 6px",
  cursor: "pointer",
  fontFamily: "monospace",
  fontSize: 12,
};

const fmtDuration = (s: number): string => {
  const total = Math.max(0, Math.round(s));
  const m = Math.floor(total / 60);
  const sec = total % 60;
  return `${m}:${sec.toString().padStart(2, "0")}`;
};

/**
 * Derive a human title from a library path. v0.1 stores no metadata
 * — the filename is the only label we have. Falls back to the id
 * when there's nothing usable.
 */
const titleOf = (track: LibraryTrack): string => {
  const stem = track.path.split("/").pop() ?? track.id;
  // Strip extension for display only — the underlying ``path`` keeps it.
  const dot = stem.lastIndexOf(".");
  return dot > 0 ? stem.slice(0, dot) : stem;
};

const submitDeckLoad = (
  client: JsonRpcWS,
  deck: DeckId,
  track: LibraryTrack,
): Promise<unknown> => {
  // Externally-tagged enum — matches the Rust `EventKind::DeckLoad`
  // serde representation (see engine/src/state.rs).
  return client.call("submit_event", {
    DeckLoad: {
      deck,
      track: { id: track.id, path: track.path },
      bpm: track.bpm,
      beat_grid_anchor_ms: track.beat_grid_anchor_ms,
      downbeats_ms: track.downbeats_ms,
      // 8-slot hot-cue grid from the library row — engine reducer
      // populates `Deck::hot_cues` directly. `Array.from` materialises
      // the readonly slice into a fresh array so JSON serialization
      // doesn't accidentally include `readonly` markers.
      hot_cues: Array.from(track.hot_cues),
    },
  });
};

export const TrackRow = ({
  track,
  client,
  onLoaded,
}: TrackRowProps): JSX.Element => {
  const handleLoad = (deck: DeckId): void => {
    // Bind the deck to this library track *before* the RPC resolves —
    // a fast HotCueSet immediately after click should still find the
    // right `track_id`. Engine roundtrip is async; persistence
    // routing isn't.
    noteLoadedTrack(deck, track.id);
    void submitDeckLoad(client, deck, track)
      .then((): void => {
        onLoaded?.(deck, track);
      })
      .catch((): void => {
        // v0.1 swallows errors — a later PR adds a toast layer.
      });
  };

  // Native HTML5 drag-source. The Deck panel registers itself as a
  // drop target (see Deck.tsx); the dataTransfer payload is the
  // serialized LibraryTrack so the drop target doesn't need to look
  // anything up.
  const handleDragStart = (e: ReactDragEvent<HTMLDivElement>): void => {
    e.dataTransfer.effectAllowed = "copy";
    e.dataTransfer.setData(
      "application/x-hypehouse-track",
      JSON.stringify(track),
    );
  };

  return (
    <div
      data-testid={`track-row-${track.id}`}
      role="listitem"
      draggable
      onDragStart={handleDragStart}
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
      </span>
    </div>
  );
};
