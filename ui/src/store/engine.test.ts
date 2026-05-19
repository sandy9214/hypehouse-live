// engine.test.ts — covers `applyNotification`'s handling of the
// master-limiter fields plumbed through the `engine.state_changed`
// payload. The reducer-side mirror of these fields lives on
// `EngineState`; the side-channel `master_limiter_gain_reduction_db`
// rides on the envelope alongside `state`.

import { afterEach, describe, expect, it } from "vitest";
import {
  __resetEngineState,
  __resetNowForTest,
  __setNowForTest,
  applyNotification,
  extrapolatedPosition,
  setDeckDuration,
  useEngineState,
} from "./engine";
import { renderHook } from "@testing-library/react";

const emit = (params: Record<string, unknown>): void => {
  applyNotification({
    jsonrpc: "2.0",
    method: "engine.state_changed",
    params,
  });
};

describe("engine store — master limiter", () => {
  afterEach((): void => {
    __resetEngineState();
  });

  it("seeds the mirror with limiter ON + -0.5 dB threshold + 0 dB GR", (): void => {
    const { result } = renderHook(() => useEngineState());
    expect(result.current.master_limiter_enabled).toBe(true);
    expect(result.current.master_limiter_threshold_db).toBeCloseTo(-0.5, 3);
    expect(result.current.master_limiter_gain_reduction_db).toBe(0);
  });

  it("absorbs limiter fields from state_changed.state", (): void => {
    emit({
      state: {
        master_limiter_enabled: false,
        master_limiter_threshold_db: -6,
      },
      last_event_id: 7,
    });
    const { result } = renderHook(() => useEngineState());
    expect(result.current.master_limiter_enabled).toBe(false);
    expect(result.current.master_limiter_threshold_db).toBe(-6);
    expect(result.current.last_event_id).toBe(7);
  });

  it("absorbs gain reduction from the envelope, not from state", (): void => {
    // GR is published OUTSIDE `state` because it's a live audio-thread
    // measurement, not part of the event-sourced reducer. Make sure the
    // store reads it from the envelope.
    emit({
      state: {},
      last_event_id: 1,
      master_limiter_gain_reduction_db: -3.2,
    });
    const { result } = renderHook(() => useEngineState());
    expect(result.current.master_limiter_gain_reduction_db).toBeCloseTo(
      -3.2,
      3,
    );
  });

  it("keeps the previous GR value when the envelope omits the field", (): void => {
    emit({
      state: {},
      last_event_id: 1,
      master_limiter_gain_reduction_db: -5,
    });
    emit({ state: {}, last_event_id: 2 }); // no GR field
    const { result } = renderHook(() => useEngineState());
    expect(result.current.master_limiter_gain_reduction_db).toBe(-5);
  });

  it("ignores notifications that aren't engine.state_changed", (): void => {
    applyNotification({
      jsonrpc: "2.0",
      method: "engine.audio_alert",
      params: { master_limiter_gain_reduction_db: -9 },
    });
    const { result } = renderHook(() => useEngineState());
    expect(result.current.master_limiter_gain_reduction_db).toBe(0);
  });
});

describe("engine store — clock source", () => {
  afterEach((): void => {
    __resetEngineState();
  });

  it("seeds the mirror with clock_source = 'internal'", (): void => {
    const { result } = renderHook(() => useEngineState());
    expect(result.current.clock_source).toBe("internal");
  });

  it("absorbs clock_source from the envelope (sibling of state)", (): void => {
    // The engine ships `clock_source` alongside `state` — same shape as
    // `master_limiter_gain_reduction_db`. The store must read it from
    // the envelope, not from `state.*`.
    emit({ state: {}, last_event_id: 1, clock_source: "midi_in" });
    const { result } = renderHook(() => useEngineState());
    expect(result.current.clock_source).toBe("midi_in");
  });

  it("keeps the previous clock_source when the envelope omits it", (): void => {
    // Sticky semantics so a partial replay (pre-PR engine binary) doesn't
    // reset the badge to INT every push.
    emit({ state: {}, last_event_id: 1, clock_source: "ableton_link" });
    emit({ state: {}, last_event_id: 2 });
    const { result } = renderHook(() => useEngineState());
    expect(result.current.clock_source).toBe("ableton_link");
  });

  it("falls back to 'internal' for an unknown clock_source variant", (): void => {
    // Defensive: a future engine variant shouldn't crash the UI. The
    // badge falls back to INT (no-lock visual) rather than rendering a
    // glitched label.
    emit({
      state: {},
      last_event_id: 1,
      clock_source: "future_variant_we_dont_know",
    });
    const { result } = renderHook(() => useEngineState());
    expect(result.current.clock_source).toBe("internal");
  });
});

describe("engine store — extrapolated position", () => {
  let mockNow = 1_000_000;
  const tick = (deltaMs: number): void => {
    mockNow += deltaMs;
  };

  afterEach((): void => {
    __resetEngineState();
    __resetNowForTest();
  });

  const emitDeckA = (
    position_ms: number,
    playing: boolean,
    tempo_ratio = 1.0,
  ): void => {
    applyNotification({
      jsonrpc: "2.0",
      method: "engine.state_changed",
      params: {
        state: {
          decks: [
            {
              id: "A",
              track_title: "demo",
              bpm: 120,
              position_ms,
              playing,
              eq_low: 0,
              eq_mid: 0,
              eq_high: 0,
              pitch_semitones: 0,
              tempo_ratio,
              hot_cues: [null, null, null, null, null, null, null, null],
              loop_in_ms: null,
              loop_out_ms: null,
              loop_active: false,
              beat_period_ms: 0,
              copilot_enabled: false,
              effects: [
                { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
                { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
                { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
              ],
            },
            {
              id: "B",
              track_title: null,
              bpm: null,
              position_ms: 0,
              playing: false,
              eq_low: 0,
              eq_mid: 0,
              eq_high: 0,
              pitch_semitones: 0,
              tempo_ratio: 1.0,
              hot_cues: [null, null, null, null, null, null, null, null],
              loop_in_ms: null,
              loop_out_ms: null,
              loop_active: false,
              beat_period_ms: 0,
              copilot_enabled: false,
              effects: [
                { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
                { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
                { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
              ],
            },
          ],
        },
        last_event_id: 1,
      },
    });
  };

  it("returns 0 when the deck has never received a state push", (): void => {
    mockNow = 1_000_000;
    __setNowForTest((): number => mockNow);
    expect(extrapolatedPosition("A")).toBe(0);
  });

  it("returns last-reported position when the deck is paused", (): void => {
    mockNow = 1_000_000;
    __setNowForTest((): number => mockNow);
    emitDeckA(12_345, false);
    tick(5_000); // wall clock advances 5s
    expect(extrapolatedPosition("A", 60_000)).toBe(12_345);
  });

  it("advances position between state_changed pushes when playing", (): void => {
    mockNow = 1_000_000;
    __setNowForTest((): number => mockNow);
    emitDeckA(10_000, true); // 10s in, playing at 1x
    tick(200); // simulate 200ms gap before next server push
    const pos = extrapolatedPosition("A", 60_000);
    expect(pos).toBe(10_200);
  });

  it("scales advancement by tempo_ratio (1.05 = +5%)", (): void => {
    mockNow = 1_000_000;
    __setNowForTest((): number => mockNow);
    emitDeckA(10_000, true, 1.05);
    tick(1_000); // 1s wall
    // 1000 × 1.05 = 1050 ms of musical time advanced.
    expect(extrapolatedPosition("A", 60_000)).toBe(11_050);
  });

  it("clamps extrapolated position to track duration", (): void => {
    mockNow = 1_000_000;
    __setNowForTest((): number => mockNow);
    emitDeckA(59_800, true);
    tick(5_000); // would advance to 64_800
    expect(extrapolatedPosition("A", 60_000)).toBe(60_000);
  });

  it("re-anchors on each state_changed so drift never accumulates", (): void => {
    mockNow = 1_000_000;
    __setNowForTest((): number => mockNow);
    emitDeckA(10_000, true);
    tick(300);
    expect(extrapolatedPosition("A", 60_000)).toBe(10_300);
    emitDeckA(11_000, true); // server says "actually at 11s"
    tick(100);
    expect(extrapolatedPosition("A", 60_000)).toBe(11_100);
  });

  it("setDeckDuration primes the anchor before any state push", (): void => {
    mockNow = 1_000_000;
    __setNowForTest((): number => mockNow);
    setDeckDuration("A", 60_000);
    // No state push yet → deck position is 0 (default), pause-snapshot.
    expect(extrapolatedPosition("A")).toBe(0);
  });

  it("never returns a negative position", (): void => {
    mockNow = 1_000_000;
    __setNowForTest((): number => mockNow);
    emitDeckA(50, false);
    // Even with a stale anchor and zero duration, never go negative.
    expect(extrapolatedPosition("A", 0)).toBeGreaterThanOrEqual(0);
  });
});
