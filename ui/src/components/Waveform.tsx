// Waveform.tsx — full-track + scrolling renderer with playhead.
//
// "full"   — whole track painted across canvas, playhead slides.
// "scroll" — pro-DJ: playhead pinned at canvas centre, peaks scroll under
//            it. Beat-grid + downbeats overlaid. rAF loop reads
//            `positionProvider` (extrapolated position from
//            store/engine.ts) so playhead stays smooth between
//            state_changed pushes (~5 Hz) at ~60 fps.
//
// Perf: peaks useMemo'd by identity; props latched into a ref so the rAF
// closure isn't re-armed on every prop change.

import { useEffect, useMemo, useRef } from "react";
import type { CSSProperties, JSX } from "react";

export type WaveformMode = "full" | "scroll";

export interface WaveformProps {
  peaks: Int8Array | null;
  positionMs?: number;
  durationMs?: number;
  height?: number;
  width?: number;
  mode?: WaveformMode;
  beatGridAnchorMs?: number;
  beatPeriodMs?: number;
  downbeatsMs?: ReadonlyArray<number>;
  /** Per-frame position provider (rAF reads this). Falls back to `positionMs`. */
  positionProvider?: () => number;
  /**
   * Compact rendering path — used by the library hover-preview row.
   * Skips the playhead, beat-grid + downbeat overlays, and bar.beat
   * label; just paints the full-track min/max peaks across the canvas
   * once. Independent of `mode` so consumers don't have to thread the
   * scroll/full distinction into a tiny preview.
   */
  compactMode?: boolean;
}

export const WAVEFORM_GRADIENT_TOP = "#6ab0ff";
export const WAVEFORM_GRADIENT_BOTTOM = "#0a2540";
export const WAVEFORM_CENTER_LINE = "#3a5070";
export const WAVEFORM_PLAYHEAD = "#ffffff";
export const WAVEFORM_BG = "#101820";
export const WAVEFORM_BG_SCROLL = "#050b18";
export const WAVEFORM_PEAK_SCROLL = "#3aa0ff";
export const WAVEFORM_BEAT = "#5a6b80";
export const WAVEFORM_DOWNBEAT = "#ffffff";
export const SCROLL_HALF_WINDOW_MS = 5_000;

/**
 * Default canvas geometry. Exported so overlay components like
 * `HotCueMarkers` can share the same width/height without duplicating
 * the magic numbers across files (council follow-up from PR #122 →
 * issue #123).
 */
export const WAVEFORM_DEFAULT_WIDTH = 480;
export const WAVEFORM_DEFAULT_HEIGHT = 96;
type Ctx = CanvasRenderingContext2D;

const canvasStyle: CSSProperties = { display: "block", background: WAVEFORM_BG };

export const peakToY = (peak: number, h: number): number =>
  Math.round((1 - (peak + 128) / 255) * h);

export const playheadX = (
  positionMs: number, durationMs: number, width: number,
): number | null => {
  if (durationMs <= 0) return null;
  const c = Math.max(0, Math.min(positionMs, durationMs));
  return Math.round((c / durationMs) * width);
};

/** ms -> canvas x in scroll mode. NaN if out of visible window. */
export const scrollXForMs = (
  ms: number, centerMs: number, width: number,
  halfWindowMs: number = SCROLL_HALF_WINDOW_MS,
): number => {
  const dx = ms - centerMs;
  if (dx < -halfWindowMs || dx > halfWindowMs) return Number.NaN;
  return width / 2 + (dx / halfWindowMs) * (width / 2);
};

/** ms -> "bar.beat" (1-indexed). */
export const barBeatLabel = (
  positionMs: number, anchorMs: number, periodMs: number,
): string => {
  if (periodMs <= 0) return "";
  const beats = Math.floor((positionMs - anchorMs) / periodMs);
  if (!Number.isFinite(beats)) return "";
  const bar = Math.floor(beats / 4) + 1;
  const beat = (((beats % 4) + 4) % 4) + 1;
  return `${bar}.${beat}`;
};

const paintBg = (ctx: Ctx, w: number, h: number, bg: string): void => {
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = bg;
  ctx.fillRect(0, 0, w, h);
  ctx.strokeStyle = WAVEFORM_CENTER_LINE;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, h / 2);
  ctx.lineTo(w, h / 2);
  ctx.stroke();
};

const drawFlat = (ctx: Ctx, w: number, h: number, bg: string): void =>
  paintBg(ctx, w, h, bg);

const paintColumn = (ctx: Ctx, peaks: Int8Array, idx: number, x: number, h: number): void => {
  const yMax = peakToY(peaks[idx * 2 + 1] ?? 0, h);
  const yMin = peakToY(peaks[idx * 2] ?? 0, h);
  if (yMax === yMin) return;
  ctx.moveTo(x + 0.5, yMax);
  ctx.lineTo(x + 0.5, yMin);
};

const drawPeaksFull = (ctx: Ctx, peaks: Int8Array, w: number, h: number): void => {
  const pairCount = Math.floor(peaks.length / 2);
  if (pairCount === 0) return drawFlat(ctx, w, h, WAVEFORM_BG);
  paintBg(ctx, w, h, WAVEFORM_BG);
  const grad = ctx.createLinearGradient(0, 0, 0, h);
  grad.addColorStop(0, WAVEFORM_GRADIENT_TOP);
  grad.addColorStop(1, WAVEFORM_GRADIENT_BOTTOM);
  ctx.strokeStyle = grad;
  ctx.beginPath();
  for (let x = 0; x < w; x++) {
    const idx = Math.min(pairCount - 1, Math.floor((x / w) * pairCount));
    paintColumn(ctx, peaks, idx, x, h);
  }
  ctx.stroke();
};

const drawPeaksScroll = (
  ctx: Ctx, peaks: Int8Array, w: number, h: number,
  centerMs: number, durationMs: number,
): void => {
  const pairCount = Math.floor(peaks.length / 2);
  if (pairCount === 0 || durationMs <= 0)
    return drawFlat(ctx, w, h, WAVEFORM_BG_SCROLL);
  paintBg(ctx, w, h, WAVEFORM_BG_SCROLL);
  ctx.strokeStyle = WAVEFORM_PEAK_SCROLL;
  ctx.beginPath();
  const msPerPx = (2 * SCROLL_HALF_WINDOW_MS) / w;
  for (let x = 0; x < w; x++) {
    const ms = centerMs + (x - w / 2) * msPerPx;
    if (ms < 0 || ms > durationMs) continue;
    const idx = Math.min(pairCount - 1, Math.floor((ms / durationMs) * pairCount));
    paintColumn(ctx, peaks, idx, x, h);
  }
  ctx.stroke();
};

const vline = (ctx: Ctx, x: number, h: number): void => {
  const xp = Math.round(x) + 0.5;
  ctx.moveTo(xp, 0);
  ctx.lineTo(xp, h);
};

const drawBeatGrid = (
  ctx: Ctx, w: number, h: number,
  centerMs: number, anchorMs: number, periodMs: number,
  downbeats: ReadonlyArray<number>,
): void => {
  if (periodMs <= 0) return;
  const startMs = centerMs - SCROLL_HALF_WINDOW_MS;
  const endMs = centerMs + SCROLL_HALF_WINDOW_MS;
  ctx.strokeStyle = WAVEFORM_BEAT;
  ctx.lineWidth = 1;
  ctx.beginPath();
  const firstN = Math.ceil((startMs - anchorMs) / periodMs);
  const lastN = Math.floor((endMs - anchorMs) / periodMs);
  for (let n = firstN; n <= lastN; n++) {
    const x = scrollXForMs(anchorMs + n * periodMs, centerMs, w);
    if (Number.isFinite(x)) vline(ctx, x, h);
  }
  ctx.stroke();
  if (downbeats.length === 0) return;
  ctx.strokeStyle = WAVEFORM_DOWNBEAT;
  ctx.lineWidth = 2;
  ctx.beginPath();
  for (const ms of downbeats) {
    if (ms < startMs || ms > endMs) continue;
    const x = scrollXForMs(ms, centerMs, w);
    if (Number.isFinite(x)) vline(ctx, x, h);
  }
  ctx.stroke();
  ctx.lineWidth = 1;
};

const drawPlayhead = (ctx: Ctx, x: number, h: number): void => {
  ctx.strokeStyle = WAVEFORM_PLAYHEAD;
  ctx.lineWidth = 1;
  ctx.beginPath();
  vline(ctx, x, h);
  ctx.stroke();
};

const labelStyle: CSSProperties = {
  position: "absolute", left: "50%", top: 2, transform: "translateX(-50%)",
  color: WAVEFORM_PLAYHEAD, fontFamily: "monospace", fontSize: 10,
  pointerEvents: "none", background: "rgba(0,0,0,0.5)",
  padding: "1px 4px", borderRadius: 2,
};

export const Waveform = ({
  peaks, positionMs = 0, durationMs = 0,
  height = WAVEFORM_DEFAULT_HEIGHT, width = WAVEFORM_DEFAULT_WIDTH,
  mode = "scroll", beatGridAnchorMs = 0, beatPeriodMs = 0,
  downbeatsMs, positionProvider, compactMode = false,
}: WaveformProps): JSX.Element => {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const labelRef = useRef<HTMLSpanElement | null>(null);
  const stablePeaks = useMemo((): Int8Array | null => peaks, [peaks]);
  const stableDownbeats = useMemo(
    (): ReadonlyArray<number> => downbeatsMs ?? [], [downbeatsMs],
  );
  const propsRef = useRef({ positionMs, durationMs, beatGridAnchorMs, beatPeriodMs, positionProvider });
  propsRef.current = { positionMs, durationMs, beatGridAnchorMs, beatPeriodMs, positionProvider };

  useEffect((): (() => void) | void => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    // Compact path: paint min/max peaks once across the canvas, no
    // playhead, no beat-grid, no rAF loop. Used for the library
    // hover preview where the row hasn't been loaded onto a deck yet
    // so there's no playhead position to render.
    if (compactMode) {
      if (!stablePeaks || stablePeaks.length === 0)
        drawFlat(ctx, width, height, WAVEFORM_BG);
      else drawPeaksFull(ctx, stablePeaks, width, height);
      return;
    }
    if (mode === "full") {
      if (!stablePeaks || stablePeaks.length === 0)
        drawFlat(ctx, width, height, WAVEFORM_BG);
      else drawPeaksFull(ctx, stablePeaks, width, height);
      const px = playheadX(propsRef.current.positionMs, propsRef.current.durationMs, width);
      if (px !== null) drawPlayhead(ctx, px, height);
      return;
    }
    let raf = 0;
    const tick = (): void => {
      const p = propsRef.current;
      const center = p.positionProvider ? p.positionProvider() : p.positionMs;
      if (!stablePeaks || stablePeaks.length === 0)
        drawFlat(ctx, width, height, WAVEFORM_BG_SCROLL);
      else drawPeaksScroll(ctx, stablePeaks, width, height, center, p.durationMs);
      drawBeatGrid(ctx, width, height, center, p.beatGridAnchorMs, p.beatPeriodMs, stableDownbeats);
      drawPlayhead(ctx, Math.round(width / 2), height);
      if (labelRef.current)
        labelRef.current.textContent = barBeatLabel(center, p.beatGridAnchorMs, p.beatPeriodMs);
      raf = requestAnimationFrame(tick);
    };
    tick();
    return (): void => cancelAnimationFrame(raf);
  }, [stablePeaks, stableDownbeats, mode, width, height, compactMode]);

  return (
    <div style={{ position: "relative", width, height, display: "inline-block" }}>
      <canvas
        ref={canvasRef}
        width={width}
        height={height}
        data-testid="waveform-canvas"
        data-mode={mode}
        data-compact={compactMode ? "true" : "false"}
        data-has-peaks={stablePeaks !== null && stablePeaks.length > 0 ? "true" : "false"}
        style={canvasStyle}
      />
      {mode === "scroll" && !compactMode ? (
        <span ref={labelRef} data-testid="waveform-barbeat" style={labelStyle} />
      ) : null}
    </div>
  );
};
