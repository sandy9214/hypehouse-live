// HotCueMarkers — overlay 8 color-coded cue dots on top of the Waveform.
//
// Engine emits `Deck.hot_cues: (u64 | null)[8]` in every state_changed
// notification (slots 0-7, ms offsets, null = unset). UI users can:
//   * Left-click a marker → jump to that cue (`HotCueTrigger` event)
//   * Right-click → delete (`HotCueClear` event)
//   * Drag (future PR) → reposition (`HotCueSet`)
//
// Layout:
//   "full"   — static positions, computed once per render.
//   "scroll" — rAF tick updates each marker's `style.left` so they
//              slide in/out of view with the waveform (positions
//              outside the visible 5 s window get `display: none`).
//
// Colors follow Rekordbox's slot-color convention (slot 0 = red,
// 1 = orange, ..., 7 = magenta) so DJs migrating from Rekordbox
// recognise the layout immediately.

import { useEffect, useRef, type CSSProperties, type JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId } from "../store/engine";
import { SCROLL_HALF_WINDOW_MS, playheadX, scrollXForMs } from "./Waveform";

export interface HotCueMarkersProps {
  readonly hotCues: ReadonlyArray<number | null>;
  readonly durationMs: number;
  readonly width: number;
  readonly height: number;
  readonly mode: "full" | "scroll";
  /** Live position — used only in scroll mode. */
  readonly positionMs?: number;
  /** Per-frame position provider (preferred over `positionMs` for smooth scroll). */
  readonly positionProvider?: () => number;
  readonly deck: DeckId;
  readonly client: JsonRpcWS;
  /** Half-window of the scroll mode in ms — defaults to the Waveform's `SCROLL_HALF_WINDOW_MS`. */
  readonly halfWindowMs?: number;
}

/**
 * Rekordbox-style slot colors. 8 entries — slot 0 → red, slot 7 → magenta.
 * Exported so tests + a future config UI can reuse the palette.
 */
export const SLOT_COLORS: ReadonlyArray<string> = [
  "#e74c3c", // red
  "#e67e22", // orange
  "#f1c40f", // yellow
  "#2ecc71", // green
  "#1abc9c", // teal
  "#3498db", // blue
  "#9b59b6", // purple
  "#e91e63", // magenta
];

const MARKER_WIDTH = 12;
const MARKER_HEIGHT = 18;

const containerStyle = (width: number, height: number): CSSProperties => ({
  position: "absolute",
  top: 0,
  left: 0,
  width,
  height,
  pointerEvents: "none",
});

const markerStyle = (color: string, height: number): CSSProperties => ({
  position: "absolute",
  top: 0,
  width: MARKER_WIDTH,
  height: MARKER_HEIGHT,
  marginLeft: -MARKER_WIDTH / 2,
  background: color,
  border: "1px solid rgba(0,0,0,0.4)",
  borderRadius: "3px 3px 0 0",
  cursor: "pointer",
  pointerEvents: "auto",
  // Drop a thin stem down the full canvas height — set via box-shadow
  // for a single DOM node per marker.
  boxShadow: `0 ${height - MARKER_HEIGHT}px 0 -${MARKER_WIDTH / 2 - 1}px ${color}`,
  color: "#000",
  fontFamily: "system-ui, sans-serif",
  fontSize: "10px",
  fontWeight: 700,
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  padding: 0,
});

/**
 * Pure positioning helper — returns the canvas-x for the cue in the
 * given mode, or `null` when the cue is offscreen / undefined.
 * Exported for unit tests.
 */
export const cueMarkerX = (
  cueMs: number | null,
  mode: "full" | "scroll",
  durationMs: number,
  centerMs: number,
  width: number,
  halfWindowMs: number,
): number | null => {
  if (cueMs === null || cueMs < 0) return null;
  if (mode === "full") return playheadX(cueMs, durationMs, width);
  const x = scrollXForMs(cueMs, centerMs, width, halfWindowMs);
  return Number.isFinite(x) ? x : null;
};

export const HotCueMarkers = ({
  hotCues,
  durationMs,
  width,
  height,
  mode,
  positionMs = 0,
  positionProvider,
  deck,
  client,
  halfWindowMs = SCROLL_HALF_WINDOW_MS,
}: HotCueMarkersProps): JSX.Element => {
  const markerRefs = useRef<(HTMLButtonElement | null)[]>(
    new Array(hotCues.length).fill(null),
  );

  // Scroll mode: rAF tick recomputes each marker's left + visibility.
  // Full mode: positions baked in at render time below; no rAF needed.
  useEffect((): (() => void) | void => {
    if (mode !== "scroll") return;
    let raf = 0;
    const tick = (): void => {
      const center = positionProvider ? positionProvider() : positionMs;
      for (let slot = 0; slot < hotCues.length; slot += 1) {
        const el = markerRefs.current[slot];
        if (!el) continue;
        const x = cueMarkerX(
          hotCues[slot] ?? null,
          "scroll",
          durationMs,
          center,
          width,
          halfWindowMs,
        );
        if (x === null) {
          el.style.display = "none";
        } else {
          el.style.display = "flex";
          el.style.left = `${x}px`;
        }
      }
      raf = requestAnimationFrame(tick);
    };
    tick();
    return (): void => cancelAnimationFrame(raf);
    // `hotCues`/`durationMs`/`width` may change as state_changed
    // notifications arrive; re-arm the rAF closure when they do.
  }, [
    mode,
    hotCues,
    durationMs,
    width,
    positionMs,
    positionProvider,
    halfWindowMs,
  ]);

  const triggerCue = (slot: number): void => {
    // Fire-and-forget — engine handles validation. We don't block the UI
    // on the resolve; the next state_changed will reflect the jump.
    void client
      .call("engine.submit_event", { HotCueTrigger: { deck, slot } })
      .catch(() => {
        // Network error path: silent for v0.1. A toast layer is
        // already wired for decode errors (Toaster.tsx); future PR can
        // route RPC errors through the same channel.
      });
  };

  const clearCue = (slot: number): void => {
    void client
      .call("engine.submit_event", { HotCueClear: { deck, slot } })
      .catch(() => {
        /* silent — see triggerCue */
      });
  };

  // Static positions for "full" mode — computed at render. Scroll mode
  // gets positions imperatively via the rAF effect above.
  const fullPositions = hotCues.map((cueMs): number | null => {
    if (mode !== "full") return null;
    return cueMarkerX(cueMs, "full", durationMs, 0, width, halfWindowMs);
  });

  return (
    <div
      style={containerStyle(width, height)}
      data-testid="hotcue-markers"
      aria-label="Hot cue markers"
    >
      {hotCues.map((cueMs, slot): JSX.Element | null => {
        // For "full" mode hide buttons that don't have a cue — for
        // "scroll" mode also hide unset cues, but the rAF tick re-tests
        // visibility every frame so don't render hidden buttons at all
        // when the cue is null (keeps DOM cheap).
        if (cueMs === null || cueMs < 0) return null;
        const color = SLOT_COLORS[slot] ?? "#ffffff";
        const initialLeft =
          mode === "full" && fullPositions[slot] !== null
            ? `${fullPositions[slot]}px`
            : "0px";
        // Slot is 0-indexed in state but commonly shown 1-indexed (the
        // "1..8" hot-cue pad row on every DJ controller). Render as
        // 1-based for the label + tooltip; the wire event still uses
        // 0-based slot — same as the keyboard fallback in
        // `midi/translator.ts`.
        const label = String(slot + 1);
        const titleText = `Cue ${label} — ${(cueMs / 1000).toFixed(2)} s. Click to jump, right-click to clear.`;
        return (
          <button
            key={slot}
            type="button"
            ref={(el): void => {
              markerRefs.current[slot] = el;
            }}
            style={{ ...markerStyle(color, height), left: initialLeft }}
            onClick={(): void => triggerCue(slot)}
            onContextMenu={(e): void => {
              e.preventDefault();
              clearCue(slot);
            }}
            aria-label={`Hot cue slot ${label} at ${cueMs} ms`}
            title={titleText}
            data-slot={slot}
            data-testid={`hotcue-marker-${slot}`}
          >
            {label}
          </button>
        );
      })}
    </div>
  );
};
