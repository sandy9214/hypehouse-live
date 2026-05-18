// perf.test.ts — covers the perf store mirror: snapshot absorption,
// rolling-history trim, and the type-safe `useStat` selector.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { renderHook } from "@testing-library/react";
import {
  __resetPerf,
  __resetPerfNowForTest,
  __setPerfNowForTest,
  applyPerfNotification,
  EMPTY_PERF_SNAPSHOT,
  PERF_HISTORY_WINDOW_MS,
  usePerf,
  usePerfHistory,
  useStat,
} from "./perf";

const emit = (perf: Record<string, unknown> | undefined): void => {
  applyPerfNotification({
    jsonrpc: "2.0",
    method: "engine.state_changed",
    params: { perf },
  });
};

describe("perf store", () => {
  beforeEach((): void => {
    __resetPerf();
    __resetPerfNowForTest();
  });
  afterEach((): void => {
    __resetPerf();
    __resetPerfNowForTest();
  });

  it("seeds the mirror with an all-zero snapshot before any push", (): void => {
    const { result } = renderHook(() => usePerf());
    expect(result.current).toEqual(EMPTY_PERF_SNAPSHOT);
  });

  it("absorbs a full perf snapshot from engine.state_changed", (): void => {
    emit({
      cpu_percent: 35.5,
      render_p99_us: 1_200,
      avg_render_us: 800,
      underrun_count: 0,
      dropped_frames: 0,
      decode_underruns: 0,
      callback_period_us: 10_667,
      render_count: 4_200,
    });
    const { result } = renderHook(() => usePerf());
    expect(result.current.cpu_percent).toBeCloseTo(35.5, 3);
    expect(result.current.render_p99_us).toBe(1_200);
    expect(result.current.render_count).toBe(4_200);
  });

  it("defaults missing perf fields to zero (forward-compat)", (): void => {
    // Older engine omits a field — the mirror absorbs what's there and
    // zero-fills the rest rather than surfacing `undefined`.
    emit({ cpu_percent: 42 });
    const { result } = renderHook(() => usePerf());
    expect(result.current.cpu_percent).toBe(42);
    expect(result.current.render_p99_us).toBe(0);
    expect(result.current.underrun_count).toBe(0);
  });

  it("rejects negative / non-finite values defensively", (): void => {
    emit({ cpu_percent: -1, render_p99_us: Number.NaN, underrun_count: 5 });
    const { result } = renderHook(() => usePerf());
    expect(result.current.cpu_percent).toBe(0);
    expect(result.current.render_p99_us).toBe(0);
    expect(result.current.underrun_count).toBe(5);
  });

  it("appends every push to the rolling history and trims by window", (): void => {
    let t = 1_000_000;
    __setPerfNowForTest((): number => t);
    emit({ cpu_percent: 10, render_p99_us: 500 });
    t += 10_000;
    emit({ cpu_percent: 20, render_p99_us: 600 });
    // Advance past the trim window — the first two points must drop off.
    t += PERF_HISTORY_WINDOW_MS + 1_000;
    emit({ cpu_percent: 30, render_p99_us: 700 });
    const { result } = renderHook(() => usePerfHistory());
    expect(result.current).toHaveLength(1);
    expect(result.current[0].cpuPercent).toBe(30);
  });

  it("useStat selector returns the named field type-safely", (): void => {
    emit({ cpu_percent: 12.3, underrun_count: 9 });
    const cpu = renderHook(() => useStat("cpu_percent"));
    expect(cpu.result.current).toBeCloseTo(12.3, 3);
    const underruns = renderHook(() => useStat("underrun_count"));
    expect(underruns.result.current).toBe(9);
  });

  it("ignores non-state_changed notifications", (): void => {
    applyPerfNotification({
      jsonrpc: "2.0",
      method: "engine.audio_alert",
      params: { kind: "xrun", details: "ignore me" },
    });
    const { result } = renderHook(() => usePerf());
    expect(result.current).toEqual(EMPTY_PERF_SNAPSHOT);
  });

  it("ignores state_changed without a perf field (older engines)", (): void => {
    applyPerfNotification({
      jsonrpc: "2.0",
      method: "engine.state_changed",
      params: { state: { decks: [] } },
    });
    const { result } = renderHook(() => usePerf());
    expect(result.current).toEqual(EMPTY_PERF_SNAPSHOT);
  });
});
