// MasterControls.test.tsx — toggle + threshold knob + meter behaviour.
//
// The component owns three side-effects:
//   1. Toggle button emits a `SetMasterLimiterEnabled` JSON-RPC event.
//   2. Threshold knob emits a `SetMasterLimiterThreshold` event.
//   3. Gain-reduction meter renders a vertical bar smoothed via rAF.
//
// We mock the `JsonRpcWS` client so each test sees the exact RPC frame
// the component sends — same shape the Rust bridge would deserialize
// (externally-tagged `EventKind` enum: `{ "VariantName": { ...payload } }`).

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { MasterControls } from "./MasterControls";

/** Minimal stub of the JSON-RPC client. We only care about `call`. */
class FakeRpc {
  public readonly calls: Array<{ method: string; params: unknown }> = [];
  public call(method: string, params?: unknown): Promise<unknown> {
    this.calls.push({ method, params });
    return Promise.resolve(null);
  }
}

interface RenderOverrides {
  enabled?: boolean;
  thresholdDb?: number;
  gainReductionDb?: number;
  meterTauMs?: number;
}

interface RenderHandle {
  client: FakeRpc;
  rerender: (next: RenderOverrides) => void;
}

const renderWith = (override: RenderOverrides = {}): RenderHandle => {
  const client = new FakeRpc();
  const props = {
    enabled: override.enabled ?? true,
    thresholdDb: override.thresholdDb ?? -0.5,
    gainReductionDb: override.gainReductionDb ?? 0,
    meterTauMs: override.meterTauMs ?? 0, // snap-to-target by default in tests
  };
  const utils = render(
    <MasterControls client={client as unknown as never} {...props} />,
  );
  return {
    client,
    rerender: (next: RenderOverrides): void => {
      const merged = { ...props, ...next };
      utils.rerender(
        <MasterControls client={client as unknown as never} {...merged} />,
      );
    },
  };
};

/** Read the fill bar's current height percentage as a number. */
const readMeterHeightPct = (): number => {
  const fill = screen.getByTestId("limiter-meter-fill");
  const h = (fill as HTMLElement).style.height; // e.g. "25.00%"
  if (!h.endsWith("%")) throw new Error(`unexpected meter height: ${h}`);
  return Number.parseFloat(h.replace("%", ""));
};

describe("MasterControls", () => {
  afterEach((): void => {
    cleanup();
  });

  it("emits SetMasterLimiterEnabled with flipped state on toggle click", (): void => {
    const { client } = renderWith({ enabled: true });
    const btn = screen.getByTestId("limiter-toggle");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(client.calls).toHaveLength(1);
    expect(client.calls[0].method).toBe("submit_event");
    expect(client.calls[0].params).toEqual({
      SetMasterLimiterEnabled: { enabled: false },
    });
  });

  it("emits SetMasterLimiterEnabled with enabled=true when toggling from OFF", (): void => {
    const { client } = renderWith({ enabled: false });
    const btn = screen.getByTestId("limiter-toggle");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(client.calls[0].params).toEqual({
      SetMasterLimiterEnabled: { enabled: true },
    });
    expect(btn.textContent).toContain("OFF");
  });

  it("emits SetMasterLimiterThreshold when the knob drags", (): void => {
    const { client } = renderWith({ thresholdDb: -0.5 });
    const input = screen.getByTestId(
      "limiter-threshold-input",
    ) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "-6" } });
    expect(client.calls).toHaveLength(1);
    expect(client.calls[0].method).toBe("submit_event");
    expect(client.calls[0].params).toEqual({
      SetMasterLimiterThreshold: { threshold_db: -6 },
    });
  });

  it("clamps the meter to zero when there is no gain reduction", (): void => {
    renderWith({ gainReductionDb: 0 });
    expect(readMeterHeightPct()).toBe(0);
    expect(screen.getByTestId("limiter-meter-readout").textContent).toContain(
      "0.0",
    );
  });

  it("fills the meter to 25% on a -3 dB reduction (METER_FLOOR_DB = -12)", (): void => {
    // 12 dB is the full-scale floor, so -3 dB → 3/12 = 25%.
    renderWith({ gainReductionDb: -3 });
    expect(readMeterHeightPct()).toBeCloseTo(25, 1);
    expect(screen.getByTestId("limiter-meter-readout").textContent).toContain(
      "-3.0",
    );
  });

  it("caps the meter fill at 100% for reductions beyond the floor", (): void => {
    renderWith({ gainReductionDb: -20 });
    expect(readMeterHeightPct()).toBe(100);
    // The numeric readout still shows the true value, not the clamped one.
    expect(screen.getByTestId("limiter-meter-readout").textContent).toContain(
      "-20.0",
    );
  });

  it("smooths meter transitions instead of snapping when tau > 0", async (): Promise<void> => {
    // Inject a deterministic rAF that advances by 16ms each frame so
    // we can step the smoother forward in synchronous-looking time.
    let nowMs = 0;
    const callbacks: Array<(t: number) => void> = [];
    const rafSpy = vi
      .spyOn(window, "requestAnimationFrame")
      .mockImplementation((cb: FrameRequestCallback): number => {
        callbacks.push(cb as (t: number) => void);
        return callbacks.length;
      });
    const cafSpy = vi
      .spyOn(window, "cancelAnimationFrame")
      .mockImplementation((): void => undefined);
    const flushFrame = (): void => {
      const ready = callbacks.splice(0, callbacks.length);
      nowMs += 16;
      for (const cb of ready) cb(nowMs);
    };

    const { rerender } = renderWith({
      gainReductionDb: 0,
      meterTauMs: 120,
    });

    // First scheduled frame primes lastFrameMsRef + paints initial 0.
    act((): void => flushFrame());
    expect(readMeterHeightPct()).toBeCloseTo(0, 1);

    // Engine reports a sudden -6 dB GR. With tau=120ms we expect the
    // meter to trend toward 50% (= 6/12) but NOT snap there in one
    // frame. After 16ms the displacement should be ~ 6 * (1 - e^(-16/120))
    // ≈ 0.75 dB → ~6.25% bar.
    rerender({ gainReductionDb: -6 });
    act((): void => flushFrame());
    const partial = readMeterHeightPct();
    expect(partial).toBeGreaterThan(0);
    expect(partial).toBeLessThan(50);

    // Step several frames forward; the meter should converge near the
    // target without overshoot.
    for (let i = 0; i < 40; i++) act((): void => flushFrame());
    const settled = readMeterHeightPct();
    expect(settled).toBeGreaterThan(45);
    expect(settled).toBeLessThanOrEqual(50.1);

    // Engine recovers to 0 dB — meter should ease back down (not snap).
    rerender({ gainReductionDb: 0 });
    act((): void => flushFrame());
    const decay1 = readMeterHeightPct();
    expect(decay1).toBeLessThan(settled);
    expect(decay1).toBeGreaterThan(0);

    rafSpy.mockRestore();
    cafSpy.mockRestore();
  });

  it("does not emit an event when the toggle button is keyboard-pressed without a click handler firing twice", (): void => {
    // Belt-and-braces: existing Button impl wires Enter/Space → onClick,
    // and we should still see exactly one event per activation.
    const { client } = renderWith({ enabled: true });
    const btn = screen.getByTestId("limiter-toggle");
    fireEvent.keyDown(btn, { key: "Enter" });
    expect(client.calls).toHaveLength(1);
    expect(client.calls[0].params).toEqual({
      SetMasterLimiterEnabled: { enabled: false },
    });
  });
});
