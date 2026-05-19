// PlaylistQueue.tsx — DJ-curated queue panel.
//
// Vertical list of queued tracks. Each row: 1-based position number,
// title + BPM + key, up/down arrows for reorder, X to remove.
// The panel is also a drop target for `application/x-hypehouse-track`
// drags from the Library, so the same library-row drag-source works
// for both Deck loads and queue enqueues.
//
// Auto-mix priority: when the queue is non-empty,
// `copilot/auto_mix.AutoMixController` dequeues the head BEFORE
// falling back to mashability ranking. See the integration test in
// `copilot/tests/test_playlist.py`.

import { useCallback } from "react";
import type { DragEvent as ReactDragEvent, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  clearPlaylist,
  enqueueTrack,
  removeTrack,
  reorderTrack,
  usePlaylist,
  type PlaylistEntry,
} from "../store/playlist";
import { playlistStyles as S } from "./PlaylistQueue.styles";

export interface PlaylistQueueProps {
  client: JsonRpcWS;
}

const titleOf = (e: PlaylistEntry): string => {
  if (e.track === null) return `${e.track_id} (missing)`;
  const stem = e.track.path.split("/").pop() ?? e.track.id;
  const dot = stem.lastIndexOf(".");
  return dot > 0 ? stem.slice(0, dot) : stem;
};

const parseDragId = (data: string): string | null => {
  if (!data) return null;
  try {
    const parsed = JSON.parse(data) as unknown;
    if (parsed && typeof parsed === "object") {
      const id = (parsed as Record<string, unknown>).id;
      if (typeof id === "string" && id) return id;
    }
  } catch {
    return null;
  }
  return null;
};

export const PlaylistQueue = ({
  client,
}: PlaylistQueueProps): JSX.Element => {
  const state = usePlaylist(client);

  const handleMove = useCallback(
    (entry: PlaylistEntry, delta: number): void => {
      const next = entry.position + delta;
      // Server clamps but skipping a no-op spares the round-trip.
      if (next < 0 || next >= state.entries.length) return;
      void reorderTrack(client, entry.track_id, next);
    },
    [client, state.entries.length],
  );

  const handleDragOver = useCallback(
    (e: ReactDragEvent<HTMLElement>): void => {
      if (e.dataTransfer.types.includes("application/x-hypehouse-track")) {
        e.preventDefault();
        e.dataTransfer.dropEffect = "copy";
      }
    },
    [],
  );

  const handleDrop = useCallback(
    (e: ReactDragEvent<HTMLElement>): void => {
      const id = parseDragId(
        e.dataTransfer.getData("application/x-hypehouse-track"),
      );
      if (id === null) return;
      e.preventDefault();
      void enqueueTrack(client, id);
    },
    [client],
  );

  const empty = state.entries.length === 0;
  const listStyle = empty ? { ...S.list, ...S.drop } : S.list;

  return (
    <section
      aria-label="Playlist queue"
      data-testid="playlist-panel"
      style={S.container}
      onDragOver={handleDragOver}
      onDrop={handleDrop}
    >
      <header style={S.header}>
        <span style={S.label}>Queue</span>
        <span data-testid="playlist-count" style={S.count}>
          {state.entries.length} track{state.entries.length === 1 ? "" : "s"}
        </span>
        {!empty && (
          <button
            type="button"
            aria-label="Clear playlist"
            data-testid="playlist-clear"
            style={S.clearBtn}
            onClick={(): void => {
              void clearPlaylist(client);
            }}
          >
            Clear
          </button>
        )}
      </header>

      {state.error !== null && (
        <div role="alert" data-testid="playlist-error" style={S.error}>
          playlist: {state.error}
        </div>
      )}

      <div role="list" style={listStyle} data-testid="playlist-list">
        {empty && (
          <div style={S.empty} data-testid="playlist-empty">
            <p>Queue is empty.</p>
            <p style={{ opacity: 0.7 }}>
              Drag a track from the library or use &quot;→ Queue&quot;
              per row. Auto-mix consumes from here first.
            </p>
          </div>
        )}
        {state.entries.map((entry: PlaylistEntry): JSX.Element => {
          const isFirst = entry.position === 0;
          const isLast = entry.position === state.entries.length - 1;
          return (
            <div
              key={`${entry.track_id}-${entry.position}`}
              role="listitem"
              data-testid={`playlist-row-${entry.track_id}`}
              style={S.row}
            >
              <span style={S.pos}>{entry.position + 1}.</span>
              <span style={S.cell} title={titleOf(entry)}>
                {titleOf(entry)}
                {entry.track === null && (
                  <span style={S.missing}>missing</span>
                )}
              </span>
              <span style={S.cell}>
                {entry.track === null ? "—" : entry.track.bpm.toFixed(1)}
              </span>
              <span style={S.cell}>
                {entry.track === null ? "—" : entry.track.camelot_key}
              </span>
              <span style={S.actions}>
                <button
                  type="button"
                  aria-label={`Move ${entry.track_id} up`}
                  data-testid={`playlist-up-${entry.track_id}`}
                  disabled={isFirst}
                  style={S.btn}
                  onClick={(): void => handleMove(entry, -1)}
                >
                  ↑
                </button>
                <button
                  type="button"
                  aria-label={`Move ${entry.track_id} down`}
                  data-testid={`playlist-down-${entry.track_id}`}
                  disabled={isLast}
                  style={S.btn}
                  onClick={(): void => handleMove(entry, 1)}
                >
                  ↓
                </button>
                <button
                  type="button"
                  aria-label={`Remove ${entry.track_id}`}
                  data-testid={`playlist-remove-${entry.track_id}`}
                  style={S.btn}
                  onClick={(): void => {
                    void removeTrack(client, entry.track_id);
                  }}
                >
                  ×
                </button>
              </span>
            </div>
          );
        })}
      </div>
    </section>
  );
};
