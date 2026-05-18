// BpmLockBadge — small "BPM source" indicator next to the master section.
//
// PR #62 wired MIDI clock IN: when an external master is providing
// tempo the engine flips `SharedClock::clock_source` to `MidiIn`, and
// the bridge surfaces that on every `engine.state_changed` envelope
// (sibling of `state`, not part of the event-sourced reducer — see
// engine/src/bridge/rpc.rs). This component renders the result:
//
//   "INT"     grey  — engine is on its own clock (default).
//   "MIDI IN" green — locked to an external MIDI master (PR #62, pulses).
//   "LINK"    cyan  — Ableton Link peer session (ADR-007 §v0.2, future).
//
// The badge sits in the master strip (next to the limiter controls)
// because the value it surfaces is master/session scope — the locked
// tempo drives BOTH decks, not just one. A per-deck placement would
// duplicate the indicator without adding information.

import type { CSSProperties } from "react";
import type { ClockSource } from "../store/engine";

export interface BpmLockBadgeProps {
  source: ClockSource;
}

interface BadgeStyle {
  label: string;
  background: string;
  border: string;
  color: string;
  pulse: boolean;
  tooltip: string;
}

/** Map every `ClockSource` to its rendered style. Centralised so the
 * styling + copy stays in one place — a future variant only needs an
 * entry here. */
const STYLE_BY_SOURCE: Record<ClockSource, BadgeStyle> = {
  internal: {
    label: "INT",
    background: "#1a1a1a",
    border: "#333",
    color: "#888",
    pulse: false,
    tooltip: "Internal clock — engine drives its own tempo",
  },
  midi_in: {
    label: "MIDI IN",
    background: "#0d2a17",
    border: "#1f7a3a",
    color: "#5bff8a",
    pulse: true,
    tooltip:
      "Locked to external MIDI clock master — master BPM is being driven by an upstream sequencer / DAW",
  },
  ableton_link: {
    label: "LINK",
    background: "#0d2030",
    border: "#1f6a8a",
    color: "#6bd6ff",
    pulse: false,
    tooltip: "Locked to Ableton Link peer session",
  },
};

const wrap: CSSProperties = {
  display: "inline-flex",
  alignItems: "center",
  gap: 4,
  padding: "2px 8px",
  borderRadius: 3,
  borderStyle: "solid",
  borderWidth: 1,
  fontFamily: "monospace",
  fontSize: 10,
  letterSpacing: 0.5,
  fontWeight: 600,
  userSelect: "none",
};

const dotStyle = (color: string, pulse: boolean): CSSProperties => ({
  width: 6,
  height: 6,
  borderRadius: "50%",
  background: color,
  // Pulse only when an external master is feeding tempo — visual
  // confirmation that the lock is live, not stale. Falls back to a
  // static dot when CSS animations are disabled.
  animation: pulse ? "bpmLockPulse 1.4s ease-in-out infinite" : "none",
  boxShadow: pulse ? `0 0 4px ${color}` : "none",
});

/** Inject the @keyframes once per page. Cheap idempotent guard —
 * `getElementById` is O(1) on a `<head>` tree this small. */
const ensureKeyframes = (): void => {
  if (typeof document === "undefined") return;
  const id = "bpm-lock-badge-keyframes";
  if (document.getElementById(id) !== null) return;
  const style = document.createElement("style");
  style.id = id;
  style.textContent =
    "@keyframes bpmLockPulse { 0%,100% { opacity: 1 } 50% { opacity: 0.45 } }";
  document.head.appendChild(style);
};

export const BpmLockBadge = ({ source }: BpmLockBadgeProps): JSX.Element => {
  ensureKeyframes();
  const style = STYLE_BY_SOURCE[source];
  const wrapStyle: CSSProperties = {
    ...wrap,
    background: style.background,
    borderColor: style.border,
    color: style.color,
  };
  return (
    <span
      style={wrapStyle}
      title={style.tooltip}
      role="status"
      aria-label={`BPM clock source: ${style.label}`}
      data-testid="bpm-lock-badge"
      data-source={source}
    >
      <span style={dotStyle(style.color, style.pulse)} aria-hidden="true" />
      <span data-testid="bpm-lock-badge-label">{style.label}</span>
    </span>
  );
};
