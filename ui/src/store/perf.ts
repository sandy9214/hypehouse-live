// Audio-thread perf store — mirror of the engine's `PerfSnapshot`.
//
// The engine stamps a fresh perf snapshot onto every `engine.state_changed`
// notification (see `engine/src/audio/perf.rs`). This store keeps:
//
//   * the latest snapshot, exposed via `usePerf()` for the gauge / badges,
//   * a 60-second rolling history of {ts, cpuPercent, renderP99Us}, exposed
//     via `usePerfHistory()` for the expanded chart in PerfDashboard.
//
// History trimming is wall-clock based (Date.now()) so a quiet engine that
// stops pushing state_changed doesn't leave stale samples on screen.

import { useSyncExternalStore } from "react";
import type { JsonRpcNotification } from "../ws/client";

/** Wire-shaped audio-thread perf payload. Mirrors
 *  `engine::audio::perf::PerfSnapshot`. Every field is non-negative. */
export interface PerfSnapshot {
  /** Average render time as a percent of the callback period.
   *  Engine-side clamp is `[0, 200]` so the gauge color logic can't be
   *  surprised by a NaN or runaway value. */
  cpu_percent: number;
  /** Per-snapshot peak render time in microseconds. Sliding window —
   *  reset by the engine on every snapshot read so this is the worst
   *  case since the last state_changed. */
  render_p99_us: number;
  /** Long-run average render time in microseconds. Persists across
   *  snapshots. */
  avg_render_us: number;
  /** cpal-side stream-error count (host underruns). */
  underrun_count: number;
  /** Recorder ring overflows in stereo frames. */
  dropped_frames: number;
  /** Decoder ring underruns (samples the audio thread asked for that
   *  the decoder couldn't supply — zero-padded with silence). */
  decode_underruns: number;
  /** cpal callback period in µs. Useful for tooltips ("render_p99 /
   *  callback_period = N%"). */
  callback_period_us: number;
  /** Total render() calls since boot. Drives the rolling-history
   *  stride more than the visual itself. */
  render_count: number;
}

/** Default empty snapshot — used before the first `engine.state_changed`
 *  lands. All zeros so the gauge reads "0% CPU" instead of "NaN%". */
export const EMPTY_PERF_SNAPSHOT: PerfSnapshot = Object.freeze({
  cpu_percent: 0,
  render_p99_us: 0,
  avg_render_us: 0,
  underrun_count: 0,
  dropped_frames: 0,
  decode_underruns: 0,
  callback_period_us: 0,
  render_count: 0,
});

interface PerfHistoryPoint {
  /** Wall-clock ms when this sample arrived (used for the 60s window
   *  trim — NOT the engine's clock). */
  ts: number;
  cpuPercent: number;
  renderP99Us: number;
}

/** Window the rolling-history chart covers. The expanded PerfDashboard
 *  view uses this as both the canvas X-axis and the trim threshold so
 *  the chart never grows unbounded. */
export const PERF_HISTORY_WINDOW_MS = 60_000;

/** Cap on the number of history points retained. Defends against a
 *  pathological engine that pushes thousands of state_changed in a 60s
 *  burst — at 5 Hz the natural cadence is ~300 points, so 2000 is a
 *  10× safety margin. */
const PERF_HISTORY_MAX_POINTS = 2_000;

let currentSnapshot: PerfSnapshot = EMPTY_PERF_SNAPSHOT;
let history: ReadonlyArray<PerfHistoryPoint> = [];

type Listener = () => void;
const listeners = new Set<Listener>();

/** Override for `now()` so tests can pin wall-clock and advance
 *  history deterministically. Same pattern the engine store uses. */
let nowFn: () => number = (): number => Date.now();
export const __setPerfNowForTest = (fn: () => number): void => {
  nowFn = fn;
};
export const __resetPerfNowForTest = (): void => {
  nowFn = (): number => Date.now();
};

const subscribe = (l: Listener): (() => void) => {
  listeners.add(l);
  return (): void => {
    listeners.delete(l);
  };
};

const getSnapshot = (): PerfSnapshot => currentSnapshot;
const getHistory = (): ReadonlyArray<PerfHistoryPoint> => history;

const notifyListeners = (): void => {
  for (const l of listeners) l();
};

/** Type-safe coercion from the wire payload. Each field falls back to
 *  the empty snapshot so a partial payload (e.g. older engine that
 *  omits a field) still produces a well-formed snapshot. */
const coercePerf = (raw: unknown): PerfSnapshot => {
  if (!raw || typeof raw !== "object") return EMPTY_PERF_SNAPSHOT;
  const r = raw as Record<string, unknown>;
  const num = (k: string): number => {
    const v = r[k];
    return typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : 0;
  };
  return {
    cpu_percent: num("cpu_percent"),
    render_p99_us: num("render_p99_us"),
    avg_render_us: num("avg_render_us"),
    underrun_count: num("underrun_count"),
    dropped_frames: num("dropped_frames"),
    decode_underruns: num("decode_underruns"),
    callback_period_us: num("callback_period_us"),
    render_count: num("render_count"),
  };
};

interface StateChangedPayload {
  perf?: unknown;
}

/** Apply a server-pushed notification. Only `engine.state_changed`
 *  carries a perf snapshot today; everything else is silently ignored. */
export const applyPerfNotification = (n: JsonRpcNotification): void => {
  if (n.method !== "engine.state_changed") return;
  const params = n.params as StateChangedPayload | undefined;
  if (!params || params.perf === undefined) return;
  const snap = coercePerf(params.perf);
  currentSnapshot = snap;
  // Append to rolling history + trim by wall-clock window. We don't
  // try to dedupe identical points — the chart's X-axis already paces
  // them out at ~16 ms granularity (the canvas pixel resolution), and
  // skipping that bookkeeping keeps the apply path O(1) amortised.
  const now = nowFn();
  const pruneBefore = now - PERF_HISTORY_WINDOW_MS;
  // Build the next array with the new point, then drop anything older
  // than the window. Two passes (filter + push) so we end up with a
  // single fresh array — `history` is treated as frozen externally.
  const trimmed = history.filter((p): boolean => p.ts >= pruneBefore);
  trimmed.push({
    ts: now,
    cpuPercent: snap.cpu_percent,
    renderP99Us: snap.render_p99_us,
  });
  // Hard cap defends against a bursty engine pushing thousands of
  // points in a single 60s window.
  history =
    trimmed.length > PERF_HISTORY_MAX_POINTS
      ? trimmed.slice(trimmed.length - PERF_HISTORY_MAX_POINTS)
      : trimmed;
  notifyListeners();
};

/** Top-level perf hook — returns the latest snapshot. Components that
 *  only need the current numbers (gauge, underrun badge) use this. */
export const usePerf = (): PerfSnapshot =>
  useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

/** Rolling-history hook — returns the trimmed-to-60s point array.
 *  Drives the expanded PerfDashboard chart. */
export const usePerfHistory = (): ReadonlyArray<PerfHistoryPoint> =>
  useSyncExternalStore(subscribe, getHistory, getHistory);

/**
 * Single-metric selector — `useStat(metric)` reads a named numeric field
 * off the latest snapshot. Convenience wrapper around `usePerf()` for
 * components that only watch one number (e.g. an underrun badge).
 *
 * The function key (not a string lookup at render time) keeps the call
 * type-safe and lets the type-checker reject typos at compile time.
 */
export const useStat = <K extends keyof PerfSnapshot>(metric: K): PerfSnapshot[K] => {
  const snap = usePerf();
  return snap[metric];
};

/** Test helper — reset both the snapshot and the history. */
export const __resetPerf = (): void => {
  currentSnapshot = EMPTY_PERF_SNAPSHOT;
  history = [];
  notifyListeners();
};

export type { PerfHistoryPoint };
