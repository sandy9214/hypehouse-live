// SessionsExportButton.tsx — per-row "Export crowd-pleaser" button.
//
// Lives in its own file so Sessions.tsx stays under the 250-line cap. The
// component owns its own progress state (idle / running / done / error)
// and surfaces the export summary as a toast-style banner alongside a
// "saved to <path>" hint.

import { useState } from "react";
import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import { exportSession, type ExportSummary } from "../store/sessions";

export interface SessionsExportButtonProps {
  client: JsonRpcWS;
  sessionId: string;
  /** When false, the recording is missing — the button stays disabled. */
  hasRecording: boolean;
}

type ExportPhase =
  | { kind: "idle" }
  | { kind: "running" }
  | { kind: "done"; summary: ExportSummary }
  | { kind: "error"; message: string };

const buttonStyle: CSSProperties = {
  background: "#1c3d22",
  color: "#cfffd6",
  border: "1px solid #2c613a",
  borderRadius: 3,
  padding: "3px 10px",
  fontSize: 11,
  cursor: "pointer",
  fontFamily: "monospace",
};

const buttonDisabledStyle: CSSProperties = {
  ...buttonStyle,
  background: "#1f1f1f",
  color: "#666",
  borderColor: "#2a2a2a",
  cursor: "not-allowed",
};

const toastStyle: CSSProperties = {
  marginTop: 6,
  padding: "4px 8px",
  fontSize: 11,
  borderRadius: 3,
  background: "#172a17",
  color: "#cfffd6",
  border: "1px solid #25502b",
};

const toastErrorStyle: CSSProperties = {
  ...toastStyle,
  background: "#3a1a1a",
  color: "#ffb0b0",
  borderColor: "#5a2727",
};

const progressStyle: CSSProperties = {
  ...toastStyle,
  background: "#1b232f",
  color: "#bcd1ea",
  borderColor: "#2a3950",
};

export const SessionsExportButton = ({
  client,
  sessionId,
  hasRecording,
}: SessionsExportButtonProps): JSX.Element => {
  const [phase, setPhase] = useState<ExportPhase>({ kind: "idle" });

  const onClick = async (): Promise<void> => {
    if (!hasRecording) return;
    if (phase.kind === "running") return;
    setPhase({ kind: "running" });
    const result = await exportSession(client, sessionId);
    if ("error" in result) {
      setPhase({ kind: "error", message: result.error });
      return;
    }
    setPhase({ kind: "done", summary: result });
  };

  return (
    <div data-testid={`sessions-export-${sessionId}`}>
      <button
        type="button"
        disabled={!hasRecording || phase.kind === "running"}
        style={hasRecording && phase.kind !== "running" ? buttonStyle : buttonDisabledStyle}
        onClick={(): void => {
          void onClick();
        }}
        data-testid={`sessions-export-btn-${sessionId}`}
        title={
          hasRecording
            ? "Trim silence + emit chapter markers for this session"
            : "No master.wav recorded for this session"
        }
      >
        {phase.kind === "running"
          ? "Exporting…"
          : phase.kind === "done"
          ? "Re-export"
          : "Export crowd-pleaser"}
      </button>
      {phase.kind === "running" ? (
        <div style={progressStyle} data-testid={`sessions-export-progress-${sessionId}`}>
          Trimming silence + writing chapters…
        </div>
      ) : null}
      {phase.kind === "done" ? (
        <div style={toastStyle} data-testid={`sessions-export-done-${sessionId}`}>
          Saved to {phase.summary.output_path}
          {" · "}
          {formatDuration(phase.summary.output_duration_s)} kept ·{" "}
          {formatTrim(
            phase.summary.trimmed_head_s,
            phase.summary.trimmed_tail_s,
          )}{" "}
          trimmed · {phase.summary.chapter_count} chapter
          {phase.summary.chapter_count === 1 ? "" : "s"}
        </div>
      ) : null}
      {phase.kind === "error" ? (
        <div style={toastErrorStyle} data-testid={`sessions-export-error-${sessionId}`}>
          Export failed: {phase.message}
        </div>
      ) : null}
    </div>
  );
};

const formatDuration = (s: number): string => {
  if (!Number.isFinite(s) || s < 0) return "—";
  if (s < 60) return `${s.toFixed(1)}s`;
  const m = Math.floor(s / 60);
  const rem = s - m * 60;
  return `${m}m ${rem.toFixed(0).padStart(2, "0")}s`;
};

const formatTrim = (head: number, tail: number): string => {
  const h = Number.isFinite(head) ? head : 0;
  const t = Number.isFinite(tail) ? tail : 0;
  return `${h.toFixed(1)}s / ${t.toFixed(1)}s`;
};
