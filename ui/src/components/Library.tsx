// Library.tsx — bottom panel listing every track in the SQLite catalog.
//
// Subscribes to ``useLibrary(client)`` (which auto-fires
// ``library.list_tracks`` on first mount) and renders one
// :component:`TrackRow` per result. The search input debounces 250ms
// and calls ``library.search_tracks`` server-side so substring +
// ``key:`` + ``bpm:`` shorthand stays consistent with the CLI.
//
// Empty-state UX (per spec requirement 7): if the library returned
// zero rows and there's no active query, surface the CLI seed
// command so the operator knows how to populate it.

import { useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  hasActiveFilters,
  searchLibrary,
  useLibrary,
  useLibraryFilters,
  type LibraryTrack,
} from "../store/library";
import { LibraryFilters } from "./LibraryFilters";
import { TrackRow } from "./TrackRow";
import { usePendingPushIds } from "../store/sessionInfo";

export interface LibraryProps {
  client: JsonRpcWS;
  // 250ms is the default debounce — tests override it to 0 so the
  // assertion fires synchronously after typing.
  searchDebounceMs?: number;
}

const containerStyle: CSSProperties = {
  background: "#0c0c0c",
  borderTop: "1px solid #333",
  color: "#ddd",
  display: "flex",
  flexDirection: "column",
  minHeight: 200,
  maxHeight: 280,
  fontFamily: "monospace",
};

const headerStyle: CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 8,
  padding: "6px 8px",
  borderBottom: "1px solid #222",
  background: "#101010",
};

const headerLabelStyle: CSSProperties = {
  fontSize: 12,
  textTransform: "uppercase",
  letterSpacing: 1,
  opacity: 0.7,
};

const searchStyle: CSSProperties = {
  flex: 1,
  background: "#000",
  color: "#fff",
  border: "1px solid #333",
  borderRadius: 4,
  padding: "4px 8px",
  fontFamily: "monospace",
  fontSize: 12,
};

const columnsRowStyle: CSSProperties = {
  display: "grid",
  // Mirror the 7-col grid in `TrackRow.tsx` — the trailing column was
  // widened from 96 to 156px to fit the playlist-queue PR's
  // "→ Queue" button alongside the existing deck-load actions.
  gridTemplateColumns: "2fr 1fr 64px 56px 72px 156px",
  gap: 8,
  padding: "4px 8px",
  fontSize: 11,
  textTransform: "uppercase",
  opacity: 0.55,
  borderBottom: "1px solid #1c1c1c",
};

const listStyle: CSSProperties = {
  overflowY: "auto",
  flex: 1,
};

const emptyBoxStyle: CSSProperties = {
  padding: 16,
  textAlign: "center",
  opacity: 0.7,
  fontSize: 13,
  lineHeight: 1.5,
};

const errorBoxStyle: CSSProperties = {
  padding: "4px 8px",
  background: "#3a1a1a",
  color: "#f3c8c8",
  fontSize: 12,
};

const codeStyle: CSSProperties = {
  background: "#1a1a1a",
  padding: "2px 6px",
  borderRadius: 3,
  fontSize: 12,
};

export const Library = ({
  client,
  searchDebounceMs = 250,
}: LibraryProps): JSX.Element => {
  const lib = useLibrary(client);
  const filters = useLibraryFilters();
  const pendingPush = usePendingPushIds(client);
  const [query, setQuery] = useState<string>("");
  const [searchResults, setSearchResults] = useState<
    ReadonlyArray<LibraryTrack> | null
  >(null);
  const debounceTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Re-search whenever EITHER the typed query or the active filter
  // state changes. We treat any active filter as a search trigger so
  // an empty-string query + "BPM 124-130" still narrows the list
  // (would otherwise fall back to the full cache).
  useEffect((): (() => void) => {
    if (debounceTimer.current !== null) {
      clearTimeout(debounceTimer.current);
      debounceTimer.current = null;
    }
    const trimmed = query.trim();
    const filtersActive = hasActiveFilters(filters);
    if (!trimmed && !filtersActive) {
      // Empty query AND no chip filters — clear overlay so the full
      // library cache shows again.
      setSearchResults(null);
      return (): void => {
        if (debounceTimer.current !== null) {
          clearTimeout(debounceTimer.current);
          debounceTimer.current = null;
        }
      };
    }
    debounceTimer.current = setTimeout((): void => {
      void searchLibrary(client, trimmed, { filters }).then(
        (rows: ReadonlyArray<LibraryTrack>): void => {
          setSearchResults(rows);
        },
      );
    }, searchDebounceMs);
    return (): void => {
      if (debounceTimer.current !== null) {
        clearTimeout(debounceTimer.current);
        debounceTimer.current = null;
      }
    };
  }, [query, client, searchDebounceMs, filters]);

  // What to actually render: search overlay if active, else the cache.
  // After the server-side narrowing, the optional `pendingSyncOnly`
  // chip post-filters client-side against the polled pending-push
  // set — the search RPC has no knowledge of cloud-sync queue state.
  const visible = useMemo<ReadonlyArray<LibraryTrack>>(
    (): ReadonlyArray<LibraryTrack> => {
      const base = searchResults ?? lib.tracks;
      if (!filters.pendingSyncOnly) return base;
      return base.filter((t: LibraryTrack): boolean =>
        pendingPush.has(t.id),
      );
    },
    [searchResults, lib.tracks, filters.pendingSyncOnly, pendingPush],
  );

  const filtersActive = hasActiveFilters(filters);
  const showEmptyState =
    lib.loaded &&
    lib.tracks.length === 0 &&
    query.trim() === "" &&
    !filtersActive;
  const showNoMatches =
    lib.loaded && lib.tracks.length > 0 && visible.length === 0;

  return (
    <section
      aria-label="Track library"
      data-testid="library-panel"
      style={containerStyle}
    >
      <header style={headerStyle}>
        <span style={headerLabelStyle}>Library</span>
        <input
          type="search"
          placeholder="search title / id / key:8B / bpm:120-130"
          aria-label="Search tracks"
          data-testid="library-search"
          value={query}
          onChange={(e): void => setQuery(e.target.value)}
          style={searchStyle}
        />
        <span
          data-testid="library-total"
          style={{ fontSize: 12, opacity: 0.6 }}
        >
          {lib.total} track{lib.total === 1 ? "" : "s"}
        </span>
      </header>

      {lib.error !== null && (
        <div role="alert" data-testid="library-error" style={errorBoxStyle}>
          library: {lib.error}
        </div>
      )}

      <LibraryFilters client={client} />

      <div style={columnsRowStyle}>
        <span>Title</span>
        <span>ID</span>
        <span>BPM</span>
        <span>Key</span>
        <span>Dur</span>
        <span></span>
      </div>

      <div role="list" style={listStyle} data-testid="library-list">
        {showEmptyState && (
          <div style={emptyBoxStyle} data-testid="library-empty">
            <p>The library is empty.</p>
            <p>
              Seed it from the CLI:{" "}
              <code style={codeStyle}>
                python -m copilot.library add /path/to/music
              </code>
            </p>
            <p style={{ opacity: 0.7 }}>
              (or from a Python REPL,{" "}
              <code style={codeStyle}>
                TrackLibrary().add_tracks_from_directory(&quot;/path&quot;)
              </code>
              )
            </p>
          </div>
        )}
        {showNoMatches && (
          <div style={emptyBoxStyle} data-testid="library-no-matches">
            {filters.pendingSyncOnly && query.trim() === ""
              ? "No tracks are pending cloud sync."
              : `No tracks match "${query}".`}
          </div>
        )}
        {visible.map(
          (t: LibraryTrack): JSX.Element => (
            <TrackRow
              key={t.id}
              track={t}
              client={client}
              pendingSync={pendingPush.has(t.id)}
            />
          ),
        )}
      </div>
    </section>
  );
};
