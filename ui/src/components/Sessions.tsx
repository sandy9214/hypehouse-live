// Sessions.tsx — History panel listing past session event logs.
//
// Renders the result of `useSessions(client)` as a scrollable table:
// session id, started timestamp, duration, event count, has-recording
// badge, plus a per-row "Export crowd-pleaser" button. Click a row to
// fetch + render the replayed `EngineState` for that session in a
// read-only details pane.
//
// Styles live in `Sessions.styles.ts`, the export-button widget lives
// in `SessionsExportButton.tsx`, and the replay snapshot pane lives in
// `SessionsSnapshotPane.tsx` so this file stays under the 250-line cap.

import { useMemo, useState } from "react";
import type { JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  formatBytes,
  formatDurationMicros,
  formatTimestampMicros,
  useSessions,
  type SessionSummary,
} from "../store/sessions";
import { SessionsExportButton } from "./SessionsExportButton";
import { SessionsSnapshotPane } from "./SessionsSnapshotPane";
import {
  bodyStyle,
  columnsRowStyle,
  containerStyle,
  detailColStyle,
  emptyStyle,
  errorBannerStyle,
  headerStyle,
  listColStyle,
  recBadgeMissingStyle,
  recBadgeStyle,
  rowBaseStyle,
  rowSelectedStyle,
} from "./Sessions.styles";

export interface SessionsProps {
  client: JsonRpcWS;
}

const SessionRow = ({
  client,
  session,
  selected,
  onSelect,
}: {
  client: JsonRpcWS;
  session: SessionSummary;
  selected: boolean;
  onSelect: (id: string) => void;
}): JSX.Element => (
  <div
    role="button"
    tabIndex={0}
    data-testid={`sessions-row-${session.id}`}
    style={selected ? rowSelectedStyle : rowBaseStyle}
    onClick={(): void => onSelect(session.id)}
    onKeyDown={(e): void => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        onSelect(session.id);
      }
    }}
  >
    <span
      title={session.id}
      style={{
        overflow: "hidden",
        textOverflow: "ellipsis",
        whiteSpace: "nowrap",
      }}
    >
      {session.id}
    </span>
    <span>{formatTimestampMicros(session.started_at_micros)}</span>
    <span>
      {formatDurationMicros(session.started_at_micros, session.ended_at_micros)}
    </span>
    <span>{session.event_count}</span>
    <span>
      {session.has_recording ? (
        <span style={recBadgeStyle} title="master.wav present">
          ●&nbsp;{formatBytes(session.recording_size_bytes)}
        </span>
      ) : (
        <span style={recBadgeMissingStyle}>none</span>
      )}
    </span>
    <span
      onClick={(e): void => e.stopPropagation()}
      onKeyDown={(e): void => e.stopPropagation()}
      role="presentation"
    >
      <SessionsExportButton
        client={client}
        sessionId={session.id}
        hasRecording={session.has_recording}
      />
    </span>
  </div>
);

export const Sessions = ({ client }: SessionsProps): JSX.Element => {
  const state = useSessions(client);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const selected = useMemo<SessionSummary | null>(
    () =>
      state.sessions.find((s: SessionSummary): boolean => s.id === selectedId) ??
      null,
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
            <span>Actions</span>
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
          {state.sessions.map((s: SessionSummary): JSX.Element => (
            <SessionRow
              key={s.id}
              client={client}
              session={s}
              selected={s.id === selectedId}
              onSelect={setSelectedId}
            />
          ))}
        </div>
        <div style={detailColStyle}>
          <SessionsSnapshotPane client={client} session={selected} />
        </div>
      </div>
    </div>
  );
};
