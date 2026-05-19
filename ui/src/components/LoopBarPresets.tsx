// LoopBarPresets — pro-DJ bar-aware auto-loop row.
//
// Pioneer-style "Loop 1 / 2 / 4 / 8 / 16" pad row. Clicking a button
// fires `EventKind::SetLoopBars`; the engine snaps `loop_in_ms` to the
// next downbeat and computes `loop_out_ms = bars × 4 × beat_period_ms`.
// See `engine/src/state.rs` (search `EventKind::SetLoopBars`).
//
// Highlighting derives from the current loop length: round-trip from
// the engine, we don't echo the requested `bars` separately so the UI
// stays a pure projection of engine state (event-sourced contract).

import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { Deck as DeckState } from "../store/engine";

/** Canonical bar-length presets. Mirror of
 *  `EngineState::LOOP_BAR_PRESETS` in `engine/src/state.rs`. */
export const LOOP_BAR_PRESETS: readonly [1, 2, 4, 8, 16] = [
  1, 2, 4, 8, 16,
] as const;

export type LoopBars = (typeof LOOP_BAR_PRESETS)[number];

/**
 * Tolerance (ms) used when matching the current loop length against
 * one of the canonical bar presets. The engine rounds the computed
 * length to integer ms; small `beat_period_ms` precision wobble can
 * still drift `bars × 4 × beat_period_ms` by ±1 ms across decks.
 * 8ms covers that comfortably without false-positive matches between
 * adjacent presets — 1-bar @ 200 BPM = 1200 ms, half of that is 600 ms.
 */
const MATCH_TOLERANCE_MS = 8;

/**
 * Determine which preset (if any) matches the deck's currently-armed
 * loop. Returns `null` when the loop is inactive, when no track is
 * loaded (no beat grid), or when the loop length doesn't match any
 * preset within `MATCH_TOLERANCE_MS` (e.g. a manual `LoopIn` →
 * `LoopOut` set the bounds outside the bar grid).
 *
 * Exported for unit testing — keeping the matcher pure makes the
 * highlighting logic auditable without rendering React.
 */
export const activeLoopBars = (deck: DeckState): LoopBars | null => {
  if (!deck.loop_active) return null;
  if (deck.loop_in_ms === null || deck.loop_out_ms === null) return null;
  if (deck.beat_period_ms <= 0) return null;
  const lengthMs = deck.loop_out_ms - deck.loop_in_ms;
  if (lengthMs <= 0) return null;
  const barMs = deck.beat_period_ms * 4;
  // Pro-DJ convention is 4/4 time signature; one bar = 4 beats.
  for (const bars of LOOP_BAR_PRESETS) {
    if (Math.abs(lengthMs - bars * barMs) <= MATCH_TOLERANCE_MS) {
      return bars;
    }
  }
  return null;
};

const rowStyle: CSSProperties = {
  display: "flex",
  gap: 4,
  alignItems: "center",
};

const labelStyle: CSSProperties = {
  fontSize: 11,
  color: "#888",
  fontFamily: "monospace",
  marginRight: 4,
};

const baseButton: CSSProperties = {
  background: "#222",
  color: "#ddd",
  border: "1px solid #444",
  borderRadius: 4,
  padding: "2px 8px",
  fontFamily: "monospace",
  fontSize: 12,
  cursor: "pointer",
  minWidth: 28,
};

const activeButton: CSSProperties = {
  ...baseButton,
  background: "#3a6",
  borderColor: "#5c8",
  color: "#fff",
};

const disabledButton: CSSProperties = {
  ...baseButton,
  cursor: "not-allowed",
  opacity: 0.5,
};

export interface LoopBarPresetsProps {
  deck: DeckState;
  client: JsonRpcWS;
}

/**
 * Render 5 pad-style buttons "1 2 4 8 16". Disabled until the deck has
 * a beat grid (`beat_period_ms > 0`) — the engine no-ops the event
 * otherwise so disabling here saves a redundant RPC round-trip and
 * makes the unavailable state visible to the DJ.
 */
export const LoopBarPresets = ({
  deck,
  client,
}: LoopBarPresetsProps): JSX.Element => {
  const hasGrid = deck.beat_period_ms > 0;
  const active = activeLoopBars(deck);
  const onClick = (bars: LoopBars): void => {
    void client
      .call("submit_event", {
        SetLoopBars: { deck: deck.id, bars },
      })
      .catch((): void => undefined);
  };
  return (
    <div
      role="group"
      aria-label={`Loop bar presets deck ${deck.id}`}
      data-testid={`loop-bar-presets-${deck.id}`}
      style={rowStyle}
    >
      <span style={labelStyle}>LOOP</span>
      {LOOP_BAR_PRESETS.map((bars): JSX.Element => {
        const isActive = active === bars;
        const style = !hasGrid
          ? disabledButton
          : isActive
            ? activeButton
            : baseButton;
        return (
          <button
            key={bars}
            type="button"
            data-testid={`loop-bars-${deck.id}-${bars}`}
            aria-label={`Set ${bars}-bar loop on deck ${deck.id}`}
            aria-pressed={isActive}
            disabled={!hasGrid}
            onClick={(): void => onClick(bars)}
            style={style}
          >
            {bars}
          </button>
        );
      })}
    </div>
  );
};
