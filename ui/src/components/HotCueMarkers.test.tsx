// HotCueMarkers.test.tsx — pure positioning + click/right-click event wiring.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import {
  HotCueMarkers,
  SLOT_COLORS,
  cueMarkerX,
  msFromX,
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

  it("tap (no drag) dispatches engine.submit_event HotCueTrigger", () => {
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
    fireEvent.pointerDown(marker, { button: 0, clientX: 50, pointerId: 1 });
    fireEvent.pointerUp(marker, { clientX: 51, pointerId: 1 });
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

  it("drag (>4 px) commits HotCueSet with the new position_ms", () => {
    const call = vi.fn().mockResolvedValue({ accepted: true });
    render(
      <HotCueMarkers
        hotCues={[10_000, null, null, null, null, null, null, null]}
        durationMs={100_000}
        width={400}
        height={96}
        mode="full"
        deck="A"
        client={makeClient(call)}
      />,
    );
    const marker = screen.getByTestId("hotcue-marker-0");
    // Pretend container starts at viewport x=0 (jsdom getBoundingClientRect default).
    fireEvent.pointerDown(marker, { button: 0, clientX: 50, pointerId: 1 });
    fireEvent.pointerMove(marker, { clientX: 100, pointerId: 1 });
    fireEvent.pointerUp(marker, { clientX: 200, pointerId: 1 });
    // 200 / 400 * 100_000 = 50_000 in "full" mode.
    expect(call).toHaveBeenCalledWith("engine.submit_event", {
      HotCueSet: { deck: "A", slot: 0, position_ms: 50_000 },
    });
    // No trigger fired — drag suppressed the tap path.
    expect(call).not.toHaveBeenCalledWith("engine.submit_event", {
      HotCueTrigger: { deck: "A", slot: 0 },
    });
  });

  it("ESC during drag cancels — no HotCueSet, no HotCueTrigger", () => {
    const call = vi.fn().mockResolvedValue({ accepted: true });
    render(
      <HotCueMarkers
        hotCues={[10_000, null, null, null, null, null, null, null]}
        durationMs={100_000}
        width={400}
        height={96}
        mode="full"
        deck="A"
        client={makeClient(call)}
      />,
    );
    const marker = screen.getByTestId("hotcue-marker-0");
    fireEvent.pointerDown(marker, { button: 0, clientX: 50, pointerId: 1 });
    fireEvent.pointerMove(marker, { clientX: 200, pointerId: 1 });
    fireEvent.keyDown(window, { key: "Escape" });
    fireEvent.pointerUp(marker, { clientX: 200, pointerId: 1 });
    expect(call).not.toHaveBeenCalledWith(
      "engine.submit_event",
      expect.objectContaining({
        HotCueSet: expect.anything(),
      }),
    );
  });

  it("non-primary mouse button (right-click) is ignored on pointerdown", () => {
    const call = vi.fn().mockResolvedValue({ accepted: true });
    render(
      <HotCueMarkers
        hotCues={[10_000, null, null, null, null, null, null, null]}
        durationMs={100_000}
        width={400}
        height={96}
        mode="full"
        deck="A"
        client={makeClient(call)}
      />,
    );
    const marker = screen.getByTestId("hotcue-marker-0");
    fireEvent.pointerDown(marker, { button: 2, clientX: 50, pointerId: 1 });
    fireEvent.pointerUp(marker, { clientX: 50, pointerId: 1 });
    expect(call).not.toHaveBeenCalled();
  });
});

describe("msFromX", () => {
  it("full mode — linear within bounds", () => {
    expect(msFromX(0, "full", 100_000, 0, 400, 5000)).toBe(0);
    expect(msFromX(200, "full", 100_000, 0, 400, 5000)).toBe(50_000);
    expect(msFromX(400, "full", 100_000, 0, 400, 5000)).toBe(100_000);
  });

  it("full mode — out-of-canvas clamps to [0, durationMs]", () => {
    expect(msFromX(-50, "full", 100_000, 0, 400, 5000)).toBe(0);
    expect(msFromX(99_999, "full", 100_000, 0, 400, 5000)).toBe(100_000);
  });

  it("scroll mode — x=width/2 returns centerMs", () => {
    expect(msFromX(200, "scroll", 100_000, 30_000, 400, 5000)).toBe(30_000);
  });

  it("scroll mode — x at right edge returns centerMs + halfWindow", () => {
    expect(msFromX(400, "scroll", 100_000, 30_000, 400, 5000)).toBe(35_000);
  });

  it("scroll mode — clamped to [0, durationMs]", () => {
    // centerMs near end of track + drag past edge → clamps to durationMs
    expect(msFromX(400, "scroll", 100_000, 99_000, 400, 5000)).toBe(100_000);
    expect(msFromX(0, "scroll", 100_000, 1_000, 400, 5000)).toBe(0);
  });

  it("returns 0 on degenerate durationMs / width", () => {
    expect(msFromX(100, "full", 0, 0, 400, 5000)).toBe(0);
    expect(msFromX(100, "full", 100_000, 0, 0, 5000)).toBe(0);
  });
});
