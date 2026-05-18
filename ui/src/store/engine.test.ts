// engine.test.ts — covers `applyNotification`'s handling of the
// master-limiter fields plumbed through the `engine.state_changed`
// payload. The reducer-side mirror of these fields lives on
// `EngineState`; the side-channel `master_limiter_gain_reduction_db`
// rides on the envelope alongside `state`.

import { afterEach, describe, expect, it } from "vitest";
import {
  __resetEngineState,
  applyNotification,
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
