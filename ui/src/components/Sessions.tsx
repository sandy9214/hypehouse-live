// Sessions.tsx — History panel listing past session event logs.
//
// Renders the result of `useSessions(client)` as a scrollable table:
// session id, started timestamp, duration, event count, has-recording
// badge. Click a row to fetch + render the replayed `EngineState` for
// that session in a read-only details pane.
//
// v0.1 deliberately does NOT mutate live engine state when replaying;
// the snapshot is purely an inspection surface. The "Replay state"
// button re-fetches the snapshot (forces a fresh RPC even if cached);
// the row's primary click hits the cache.
//
// The panel sits behind a History toggle in `App.tsx` — DJs primarily
// care about live mixing, so we keep this utility surface secondary.

import { useMemo, useState } from "react";
import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  fetchReplay,
  formatBytes,
  formatDurationMicros,
  formatTimestampMicros,
  useReplay,
  useSessions,
  type ReplayedEngineState,
  type SessionSummary,
} from "../store/sessions";

export interface SessionsProps {
  client: JsonRpcWS;
}

const containerStyle: CSSProperties = {
  background: "#0c0c0c",
  color: "#ddd",
  display: "flex",
  flexDirection: "column",
  flex: 1,
  minHeight: 0,
  fontFamily: "monospace",
};

const headerStyle: CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 8,
  padding: "8px 10px",
  borderBottom: "1px solid #222",
  background: "#101010",
  fontSize: 12,
  textTransform: "uppercase",
  letterSpacing: 1,
  opacity: 0.85,
};

const bodyStyle: CSSProperties = {
  display: "flex",
  flex: 1,
  minHeight: 0,
};

const listColStyle: CSSProperties = {
  flex: 1,
  minWidth: 0,
  overflowY: "auto",
  borderRight: "1px solid #222",
};

const detailColStyle: CSSProperties = {
  flex: 1,
  minWidth: 0,
  overflowY: "auto",
  padding: "10px 12px",
  fontSize: 12,
};

const columnsRowStyle: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "1.7fr 1.3fr 0.8fr 0.7fr 0.9fr",
  gap: 8,
  padding: "6px 10px",
  fontSize: 11,
  textTransform: "uppercase",
  opacity: 0.55,
  borderBottom: "1px solid #1c1c1c",
  background: "#0a0a0a",
};

const rowBaseStyle: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "1.7fr 1.3fr 0.8fr 0.7fr 0.9fr",
  gap: 8,
  padding: "6px 10px",
  fontSize: 12,
  borderBottom: "1px solid #161616",
  cursor: "pointer",
  background: "transparent",
};

const rowSelectedStyle: CSSProperties = {
  ...rowBaseStyle,
  background: "#19283a",
};

const recBadgeStyle: CSSProperties = {
  display: "inline-block",
  padding: "1px 6px",
  borderRadius: 3,
  fontSize: 10,
  background: "#274028",
  color: "#9ee29a",
  border: "1px solid #2f5331",
};

const recBadgeMissingStyle: CSSProperties = {
  ...recBadgeStyle,
  background: "#1c1c1c",
  color: "#666",
  borderColor: "#2a2a2a",
};

const buttonStyle: CSSProperties = {
  background: "#1c2a3d",
  color: "#cce0ff",
  border: "1px solid #2c4361",
  borderRadius: 3,
  padding: "3px 8px",
  fontSize: 11,
  cursor: "pointer",
  fontFamily: "monospace",
};

const emptyStyle: CSSProperties = {
  padding: "16px",
  textAlign: "center",
  opacity: 0.6,
  fontSize: 13,
};

const errorBannerStyle: CSSProperties = {
  padding: "6px 10px",
  background: "#3a1a1a",
  color: "#ffb0b0",
  borderBottom: "1px solid #5a2727",
  fontSize: 12,
};

interface DeckSnapshot {
  loaded?: { id?: string; path?: string } | null;
  playing?: boolean;
  bpm?: number;
  position_ms?: number;
  hot_cues?: ReadonlyArray<number | null>;
  copilot_engaged?: boolean;
}

/** Pull a typed field out of the replayed state. */
const pickDeck = (state: ReplayedEngineState, key: string): DeckSnapshot => {
  const v = state[key];
  if (!v || typeof v !== "object") return {};
  return v as DeckSnapshot;
};

const pickNumber = (
  state: ReplayedEngineState,
  key: string,
  fallback: number,
): number => {
  const v = state[key];
  return typeof v === "number" && Number.isFinite(v) ? v : fallback;
};

const pickBoolean = (
  state: ReplayedEngineState,
  key: string,
  fallback: boolean,
): boolean => (typeof state[key] === "boolean" ? (state[key] as boolean) : fallback);

const SnapshotPane = ({
  client,
  session,
}: {
  client: JsonRpcWS;
  session: SessionSummary | null;
}): JSX.Element => {
  const state = useReplay(client, session?.id ?? null);
  if (!session) {
    return (
      <div style={emptyStyle} data-testid="sessions-pick-prompt">
        Select a session to inspect.
      </div>
    );
  }
  if (state.loading) {
    return (
      <div style={emptyStyle} data-testid="sessions-replay-loading">
        Replaying events…
      </div>
    );
  }
  if (state.error || !state.result) {
    return (
      <div style={emptyStyle} data-testid="sessions-replay-error">
        Could not replay this session.
      </div>
    );
  }
  const snap = state.result.state;
  const deckA = pickDeck(snap, "deck_a");
  const deckB = pickDeck(snap, "deck_b");
  const crossfader = pickNumber(snap, "crossfader", 0.5);
  const masterBpm = pickNumber(snap, "master_bpm", 0);
  const sessionActive = pickBoolean(snap, "session_active", false);
  return (
    <div data-testid="sessions-replay-snapshot">
      <div style={{ marginBottom: 10 }}>
        <div style={{ opacity: 0.6, fontSize: 11, marginBottom: 2 }}>
          Snapshot at last event
        </div>
        <div data-testid="sessions-replay-event-count">
          {state.result.event_count} events replayed
        </div>
      </div>
      <div style={{ marginBottom: 8 }}>
        <strong>Session active:</strong> {sessionActive ? "yes" : "no"}
        {"  "}|{"  "}
        <strong>Master BPM:</strong> {masterBpm.toFixed(2)}
        {"  "}|{"  "}
        <strong>Crossfader:</strong> {crossfader.toFixed(2)}
      </div>
      <DeckSummary label="Deck A" deck={deckA} />
      <DeckSummary label="Deck B" deck={deckB} />
      <div style={{ marginTop: 10 }}>
        <button
          type="button"
          style={buttonStyle}
          data-testid="sessions-replay-refresh"
          onClick={(): void => {
            void fetchReplay(client, session.id);
          }}
        >
          Replay state
        </button>
        {session.has_recording ? (
          <span
            style={{ marginLeft: 10, opacity: 0.7 }}
            data-testid="sessions-recording-hint"
          >
            master.wav available ({formatBytes(session.recording_size_bytes)})
          </span>
        ) : null}
      </div>
    </div>
  );
};

const DeckSummary = ({
  label,
  deck,
}: {
  label: string;
  deck: DeckSnapshot;
}): JSX.Element => {
  const trackId = deck.loaded?.id ?? "(none)";
  const hotCues = deck.hot_cues ?? [];
  const setCues = hotCues.filter((c): c is number => typeof c === "number");
  return (
    <div style={{ marginBottom: 8 }}>
      <strong>{label}</strong>: {trackId}
      {" "}
      {deck.playing ? "▶" : "⏸"}
      {" "}
      {typeof deck.bpm === "number" && deck.bpm > 0
        ? `${deck.bpm.toFixed(2)} BPM`
        : ""}
      {deck.copilot_engaged ? " · copilot" : ""}
      {" · "}
      cues set {setCues.length}/{hotCues.length || 8}
    </div>
  );
};

export const Sessions = ({ client }: SessionsProps): JSX.Element => {
  const state = useSessions(client);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const selected = useMemo<SessionSummary | null>(
    () => state.sessions.find((s: SessionSummary): boolean => s.id === selectedId) ?? null,
    [state.sessions, selectedId],
  );

  return (
    <div style={containerStyle} data-testid="sessions-panel">
      <div style={headerStyle}>
        <span>History</span>
        <span style={{ opacity: 0.5, fontSize: 11 }}>
          ({state.sessions.length})
        </span>
      </div>
      {state.error ? (
        <div style={errorBannerStyle} data-testid="sessions-error">
          {state.error}
        </div>
      ) : null}
      <div style={bodyStyle}>
        <div style={listColStyle}>
          <div style={columnsRowStyle}>
            <span>Session</span>
            <span>Started</span>
            <span>Duration</span>
            <span>Events</span>
            <span>Recording</span>
          </div>
          {state.loading && !state.loaded ? (
            <div style={emptyStyle} data-testid="sessions-loading">
              Loading sessions…
            </div>
          ) : null}
          {state.loaded && state.sessions.length === 0 && !state.error ? (
            <div style={emptyStyle} data-testid="sessions-empty">
              No past sessions yet. Start a live set to record one.
            </div>
          ) : null}
          {state.sessions.map((s: SessionSummary): JSX.Element => {
            const isSelected = s.id === selectedId;
            return (
              <div
                key={s.id}
                role="button"
                tabIndex={0}
                data-testid={`sessions-row-${s.id}`}
                style={isSelected ? rowSelectedStyle : rowBaseStyle}
                onClick={(): void => setSelectedId(s.id)}
                onKeyDown={(e): void => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    setSelectedId(s.id);
                  }
                }}
              >
                <span title={s.id} style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                  {s.id}
                </span>
                <span>{formatTimestampMicros(s.started_at_micros)}</span>
                <span>
                  {formatDurationMicros(s.started_at_micros, s.ended_at_micros)}
                </span>
                <span>{s.event_count}</span>
                <span>
                  {s.has_recording ? (
                    <span style={recBadgeStyle} title="master.wav present">
                      ●&nbsp;{formatBytes(s.recording_size_bytes)}
                    </span>
                  ) : (
                    <span style={recBadgeMissingStyle}>none</span>
                  )}
                </span>
              </div>
            );
          })}
        </div>
        <div style={detailColStyle}>
          <SnapshotPane client={client} session={selected} />
        </div>
      </div>
    </div>
  );
};
