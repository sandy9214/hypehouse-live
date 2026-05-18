// Waveform.tsx — real min/max peak-pair waveform renderer.
//
// ADR-001 keeps the audio path inside the Rust engine; the UI only
// visualises. Peaks are computed copilot-side at ingest time (see
// `copilot/waveform.py`) and fetched via `library.get_waveform` —
// this component never touches PCM directly.
//
// Render contract:
//   * Accept `peaks: Int8Array | null`. Each pair (i*2, i*2+1) is the
//     (min, max) of one time bucket, value range [-128, 127].
//   * Drawing model: per x-pixel, pick a bucket index proportional to
//     x/width, draw a vertical line from `min_y` to `max_y`. Centre
//     horizontal line at y=height/2 (the audio zero).
//   * Playhead: vertical white line at `position_ms / duration_ms * width`.
//   * No peaks (null): show a flat midline — same v0.1 fallback.
//   * Colour theme: blue gradient (light at top, dark at bottom).
//
// Why no off-screen canvas / dirty rect tracking: a 480-px-wide canvas
// at 2000 buckets is ~480 draw calls per frame, ~30 µs on modern
// hardware. The position-cursor re-draw on every state_changed tick
// is the dominant cost and we already redraw the full canvas anyway.
// Skip the complexity until profiling says otherwise.

import { useEffect, useMemo, useRef } from "react";
import type { CSSProperties, JSX } from "react";

export interface WaveformProps {
  /** Packed min/max peak pairs (2*N i8 bytes). `null` ⇒ render flat. */
  peaks: Int8Array | null;
  /** Current playhead position in milliseconds. */
  positionMs?: number;
  /** Track duration in milliseconds — used to position the playhead. */
  durationMs?: number;
  height?: number;
  width?: number;
}

// Blue gradient — light at top, dark at bottom. Pulled out so tests
// can assert the stops are present without hard-coding hex bytes
// inside the draw call.
export const WAVEFORM_GRADIENT_TOP = "#6ab0ff";
export const WAVEFORM_GRADIENT_BOTTOM = "#0a2540";
export const WAVEFORM_CENTER_LINE = "#3a5070";
export const WAVEFORM_PLAYHEAD = "#ffffff";
export const WAVEFORM_BG = "#101820";

const canvasStyle: CSSProperties = {
  display: "block",
  background: WAVEFORM_BG,
};

/**
 * Convert an i8 peak value (-128..=127) to a y-coordinate in canvas
 * space (0..=height). Pulled out so the test suite can verify the
 * mapping without rendering a real canvas.
 */
export const peakToY = (peak: number, height: number): number => {
  // Map [-128, 127] → [0, height]. Audio min (-128) draws at the
  // bottom (y = height); audio max (127) draws at the top (y = 0).
  // Visually: louder negative excursions extend downward.
  const norm = (peak + 128) / 255; // [0, 1]
  return Math.round((1 - norm) * height);
};

/**
 * Compute the x-pixel of the playhead given a position + duration.
 * Returns `null` when the math is undefined (duration ≤ 0) so the
 * caller can skip drawing rather than render at x=0.
 */
export const playheadX = (
  positionMs: number,
  durationMs: number,
  width: number,
): number | null => {
  if (durationMs <= 0) return null;
  const clamped = Math.max(0, Math.min(positionMs, durationMs));
  return Math.round((clamped / durationMs) * width);
};

const drawFlat = (
  ctx: CanvasRenderingContext2D,
  width: number,
  height: number,
): void => {
  ctx.clearRect(0, 0, width, height);
  ctx.fillStyle = WAVEFORM_BG;
  ctx.fillRect(0, 0, width, height);
  ctx.strokeStyle = WAVEFORM_CENTER_LINE;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, height / 2);
  ctx.lineTo(width, height / 2);
  ctx.stroke();
};

const drawPeaks = (
  ctx: CanvasRenderingContext2D,
  peaks: Int8Array,
  width: number,
  height: number,
): void => {
  ctx.clearRect(0, 0, width, height);
  ctx.fillStyle = WAVEFORM_BG;
  ctx.fillRect(0, 0, width, height);

  const pairCount = Math.floor(peaks.length / 2);
  if (pairCount === 0) {
    drawFlat(ctx, width, height);
    return;
  }

  // Centre line first so it sits behind the bars visually.
  ctx.strokeStyle = WAVEFORM_CENTER_LINE;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, height / 2);
  ctx.lineTo(width, height / 2);
  ctx.stroke();

  // Blue gradient — top is bright, bottom is dark. Painted into a
  // strokeStyle by building a vertical gradient. createLinearGradient
  // coordinates are canvas-space (x0, y0, x1, y1).
  const grad = ctx.createLinearGradient(0, 0, 0, height);
  grad.addColorStop(0, WAVEFORM_GRADIENT_TOP);
  grad.addColorStop(1, WAVEFORM_GRADIENT_BOTTOM);
  ctx.strokeStyle = grad;
  ctx.beginPath();
  for (let x = 0; x < width; x++) {
    // Pick the bucket index proportional to x/width. Float floor
    // gives a stable mapping for any (width, pairCount) ratio.
    const idx = Math.min(
      pairCount - 1,
      Math.floor((x / width) * pairCount),
    );
    const minPeak = peaks[idx * 2] ?? 0;
    const maxPeak = peaks[idx * 2 + 1] ?? 0;
    const yMax = peakToY(maxPeak, height); // top of bar (audio max)
    const yMin = peakToY(minPeak, height); // bottom of bar (audio min)
    // Skip degenerate buckets (silent region) — they'd paint a 1px dot
    // on the centre line which the centre-line draw already covered.
    if (yMax === yMin) continue;
    ctx.moveTo(x + 0.5, yMax);
    ctx.lineTo(x + 0.5, yMin);
  }
  ctx.stroke();
};

const drawPlayhead = (
  ctx: CanvasRenderingContext2D,
  x: number,
  height: number,
): void => {
  ctx.strokeStyle = WAVEFORM_PLAYHEAD;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(x + 0.5, 0);
  ctx.lineTo(x + 0.5, height);
  ctx.stroke();
};

export const Waveform = ({
  peaks,
  positionMs = 0,
  durationMs = 0,
  height = 96,
  width = 480,
}: WaveformProps): JSX.Element => {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  // Memoize the peaks reference so an upstream re-render that hands us
  // the same Int8Array doesn't trigger a redraw cascade. Identity is
  // enough — the array contents are immutable per ``waveform.ts``
  // cache semantics.
  const stablePeaks = useMemo((): Int8Array | null => peaks, [peaks]);

  useEffect((): void => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    if (!stablePeaks || stablePeaks.length === 0) {
      drawFlat(ctx, width, height);
    } else {
      drawPeaks(ctx, stablePeaks, width, height);
    }

    const px = playheadX(positionMs, durationMs, width);
    if (px !== null) drawPlayhead(ctx, px, height);
  }, [stablePeaks, positionMs, durationMs, width, height]);

  return (
    <canvas
      ref={canvasRef}
      width={width}
      height={height}
      data-testid="waveform-canvas"
      data-has-peaks={
        stablePeaks !== null && stablePeaks.length > 0 ? "true" : "false"
      }
      style={canvasStyle}
    />
  );
};
