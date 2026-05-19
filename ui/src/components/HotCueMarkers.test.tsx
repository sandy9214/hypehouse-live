// HotCueMarkers.test.tsx — pure positioning + click/right-click event wiring.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import {
  HotCueMarkers,
  SLOT_COLORS,
  cueMarkerX,
} from "./HotCueMarkers";
import type { JsonRpcWS } from "../ws/client";

const makeClient = (
  call: ((method: string, params?: unknown) => Promise<unknown>) | null = null,
): JsonRpcWS =>
  ({
    call: call ?? vi.fn().mockResolvedValue({ accepted: true }),
  }) as unknown as JsonRpcWS;

describe("cueMarkerX", () => {
  it("returns null on null / negative cue ms", () => {
    expect(cueMarkerX(null, "full", 100_000, 0, 400, 5000)).toBeNull();
    expect(cueMarkerX(-5, "full", 100_000, 0, 400, 5000)).toBeNull();
  });

  it("full mode proportional to durationMs", () => {
    // 50_000 ms cue inside a 100_000 ms track → center of canvas.
    expect(cueMarkerX(50_000, "full", 100_000, 0, 400, 5000)).toBe(200);
    expect(cueMarkerX(0, "full", 100_000, 0, 400, 5000)).toBe(0);
    expect(cueMarkerX(100_000, "full", 100_000, 0, 400, 5000)).toBe(400);
  });

  it("scroll mode: cue at center returns canvas centre", () => {
    expect(cueMarkerX(30_000, "scroll", 100_000, 30_000, 400, 5000)).toBe(200);
  });

  it("scroll mode: cue at center +halfWindow returns right edge", () => {
    expect(cueMarkerX(35_000, "scroll", 100_000, 30_000, 400, 5000)).toBe(400);
  });

  it("scroll mode: cue at center -halfWindow returns left edge", () => {
    expect(cueMarkerX(25_000, "scroll", 100_000, 30_000, 400, 5000)).toBe(0);
  });

  it("scroll mode: cue outside window returns null", () => {
    expect(cueMarkerX(50_000, "scroll", 100_000, 30_000, 400, 5000)).toBeNull();
  });
});

describe("HotCueMarkers", () => {
  afterEach((): void => {
    cleanup();
  });

  it("renders no buttons when all slots are null", () => {
    render(
      <HotCueMarkers
        hotCues={[null, null, null, null, null, null, null, null]}
        durationMs={100_000}
        width={400}
        height={96}
        mode="full"
        deck="A"
        client={makeClient()}
      />,
    );
    for (let i = 0; i < 8; i += 1) {
      expect(screen.queryByTestId(`hotcue-marker-${i}`)).toBeNull();
    }
  });

  it("renders only the set slots, color-coded", () => {
    render(
      <HotCueMarkers
        hotCues={[1000, null, 50_000, null, null, null, null, null]}
        durationMs={100_000}
        width={400}
        height={96}
        mode="full"
        deck="A"
        client={makeClient()}
      />,
    );
    const m0 = screen.getByTestId("hotcue-marker-0");
    const m2 = screen.getByTestId("hotcue-marker-2");
    expect(m0).toBeTruthy();
    expect(m2).toBeTruthy();
    expect(screen.queryByTestId("hotcue-marker-1")).toBeNull();
    expect(m0.style.background).toMatch(/rgb|^#/);
    expect(SLOT_COLORS.length).toBe(8);
  });

  it("left-click dispatches engine.submit_event HotCueTrigger", () => {
    const call = vi.fn().mockResolvedValue({ accepted: true });
    render(
      <HotCueMarkers
        hotCues={[1000, null, null, null, null, null, null, null]}
        durationMs={100_000}
        width={400}
        height={96}
        mode="full"
        deck="B"
        client={makeClient(call)}
      />,
    );
    const marker = screen.getByTestId("hotcue-marker-0");
    fireEvent.click(marker);
    expect(call).toHaveBeenCalledWith("engine.submit_event", {
      HotCueTrigger: { deck: "B", slot: 0 },
    });
  });

  it("right-click dispatches engine.submit_event HotCueClear and prevents default", () => {
    const call = vi.fn().mockResolvedValue({ accepted: true });
    render(
      <HotCueMarkers
        hotCues={[null, 2500, null, null, null, null, null, null]}
        durationMs={100_000}
        width={400}
        height={96}
        mode="full"
        deck="A"
        client={makeClient(call)}
      />,
    );
    const marker = screen.getByTestId("hotcue-marker-1");
    fireEvent.contextMenu(marker);
    expect(call).toHaveBeenCalledWith("engine.submit_event", {
      HotCueClear: { deck: "A", slot: 1 },
    });
  });

  it("renders 1-indexed labels even though slot is 0-indexed", () => {
    render(
      <HotCueMarkers
        hotCues={[1000, null, null, null, null, null, null, 99_000]}
        durationMs={100_000}
        width={400}
        height={96}
        mode="full"
        deck="A"
        client={makeClient()}
      />,
    );
    expect(screen.getByTestId("hotcue-marker-0").textContent).toBe("1");
    expect(screen.getByTestId("hotcue-marker-7").textContent).toBe("8");
  });
});
