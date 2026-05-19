// AboutPanel — engine version + active output device + feature flags.
//
// Backed by the new `engine.session_info` RPC (#144). Read-only,
// fetched once on mount; not subscribed to state_changed because the
// payload doesn't carry session-static fields.
//
// Useful for:
// - Bug reports — operator can copy version + flags into the issue body.
// - Sanity-check on first run — confirms BlackHole / VB-Cable was actually
//   picked up by the engine (otherwise the dropdown's persisted choice
//   silently doesn't apply).

import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  useSessionInfo,
  useSyncStatus,
  type SessionFeatures,
} from "../store/sessionInfo";

export interface AboutPanelProps {
  readonly client: JsonRpcWS;
}

const containerStyle: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: "0.3rem",
  padding: "0.6rem",
  border: "1px solid #2a2a2a",
  borderRadius: "0.4rem",
  background: "#0f0f0f",
  color: "#bbb",
  fontFamily: "system-ui, sans-serif",
  fontSize: "0.8rem",
  maxWidth: "420px",
};

const labelStyle: CSSProperties = {
  fontWeight: 600,
  color: "#888",
  letterSpacing: 0.5,
  fontSize: "0.7rem",
  textTransform: "uppercase",
};

const rowStyle: CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  gap: "0.6rem",
};

const valueStyle: CSSProperties = {
  color: "#ddd",
  fontFamily: "monospace",
  fontSize: "0.75rem",
};

const flagsRowStyle: CSSProperties = {
  display: "flex",
  flexWrap: "wrap",
  gap: "0.3rem",
  marginTop: "0.2rem",
};

const flagOnStyle: CSSProperties = {
  background: "#1f3d22",
  color: "#9ce0a3",
  border: "1px solid #2c5630",
  padding: "1px 6px",
  borderRadius: "0.25rem",
  fontSize: "0.7rem",
};

const flagOffStyle: CSSProperties = {
  background: "#1a1a1a",
  color: "#666",
  border: "1px solid #2a2a2a",
  padding: "1px 6px",
  borderRadius: "0.25rem",
  fontSize: "0.7rem",
};

const FEATURE_LABELS: ReadonlyArray<[keyof SessionFeatures, string]> = [
  ["midi_clock_in", "MIDI Clock In"],
  ["midi_clock_out", "MIDI Clock Out"],
  ["ableton_link", "Ableton Link"],
  ["sentry_telemetry", "Sentry"],
  ["recording_enabled", "Recording"],
  ["rate_limit_disabled", "Rate-limit OFF"],
  ["shared_ci_runner", "Shared CI"],
];

export const AboutPanel = ({ client }: AboutPanelProps): JSX.Element => {
  const info = useSessionInfo(client);
  const sync = useSyncStatus(client);
  const deviceLabel =
    info.output_device_substring === ""
      ? "(system default)"
      : info.output_device_substring;
  const versionLabel = info.version === "" ? "(loading…)" : info.version;

  return (
    <div style={containerStyle} data-testid="about-panel">
      <div style={rowStyle}>
        <span style={labelStyle}>Engine</span>
        <span style={valueStyle} data-testid="about-version">
          v{versionLabel}
        </span>
      </div>
      <div style={rowStyle}>
        <span style={labelStyle}>Audio sink</span>
        <span style={valueStyle} data-testid="about-output-device">
          {deviceLabel}
        </span>
      </div>
      <div style={rowStyle}>
        <span style={labelStyle}>Library</span>
        <span style={valueStyle} data-testid="about-library-count">
          {sync.library_track_count} tracks
          {sync.pending_push_count > 0
            ? ` · ${sync.pending_push_count} pending sync`
            : ""}
        </span>
      </div>
      <div style={flagsRowStyle} data-testid="about-flags">
        {FEATURE_LABELS.map(([key, label]): JSX.Element => {
          const enabled = info.features[key];
          return (
            <span
              key={key}
              style={enabled ? flagOnStyle : flagOffStyle}
              data-testid={`about-flag-${key}`}
              title={`${label}: ${enabled ? "on" : "off"}`}
            >
              {label}
            </span>
          );
        })}
      </div>
    </div>
  );
};
