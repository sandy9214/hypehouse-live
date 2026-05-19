// SessionsSnapshotPane.tsx — read-only replay snapshot rendered in the
// History panel's right column. Extracted from Sessions.tsx so the
// parent stays under the 250-line cap.

import type { JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  fetchReplay,
  formatBytes,
  useReplay,
  type ReplayedEngineState,
  type SessionSummary,
} from "../store/sessions";
import { SessionsExportButton } from "./SessionsExportButton";
import { buttonStyle, emptyStyle } from "./Sessions.styles";

interface DeckSnapshot {
  loaded?: { id?: string; path?: string } | null;
  playing?: boolean;
  bpm?: number;
  position_ms?: number;
  hot_cues?: ReadonlyArray<number | null>;
  copilot_engaged?: boolean;
}

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
      <strong>{label}</strong>: {trackId} {deck.playing ? "▶" : "⏸"}{" "}
      {typeof deck.bpm === "number" && deck.bpm > 0
        ? `${deck.bpm.toFixed(2)} BPM`
        : ""}
      {deck.copilot_engaged ? " · copilot" : ""} · cues set {setCues.length}/
      {hotCues.length || 8}
    </div>
  );
};

export const SessionsSnapshotPane = ({
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
        <strong>Session active:</strong> {sessionActive ? "yes" : "no"} |{" "}
        <strong>Master BPM:</strong> {masterBpm.toFixed(2)} |{" "}
        <strong>Crossfader:</strong> {crossfader.toFixed(2)}
      </div>
      <DeckSummary label="Deck A" deck={deckA} />
      <DeckSummary label="Deck B" deck={deckB} />
      <div style={{ marginTop: 10, display: "flex", gap: 8, alignItems: "center" }}>
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
            style={{ opacity: 0.7 }}
            data-testid="sessions-recording-hint"
          >
            master.wav available ({formatBytes(session.recording_size_bytes)})
          </span>
        ) : null}
      </div>
      <div style={{ marginTop: 10 }}>
        <SessionsExportButton
          client={client}
          sessionId={session.id}
          hasRecording={session.has_recording}
        />
      </div>
    </div>
  );
};
