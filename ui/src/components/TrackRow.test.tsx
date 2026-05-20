// TrackRow.test.tsx — hover/long-press waveform preview behaviour.
//
// The existing click-to-load path is already covered indirectly by
// Library.test.tsx; this file pins the new hover-preview flow added
// in PR ui-library-waveform-hover:
//
// - mouseenter debounces 200ms then fires `library.get_waveform`
// - mouseleave-before-debounce never fires
// - re-hover within 30s skips the second RPC
// - touchstart held 500ms fires the preview (mobile)
// - the rendered Waveform child carries the `compactMode` flag

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { TrackRow, __resetTrackRowHoverCache } from "./TrackRow";
import type { JsonRpcWS } from "../ws/client";
import type { LibraryTrack } from "../store/library";
import { __resetWaveformCache } from "../store/waveform";

interface CanvasOp { readonly op: string; readonly args: ReadonlyArray<unknown>; }

/** Minimal canvas-2d recorder identical to the one in Waveform.test.tsx
 * — pulled inline rather than shared because the shape is small and
 * we don't want a circular `import` between two test files. */
const installCanvasRecorder = (): { restore: () => void; ops: CanvasOp[] } => {
  const ops: CanvasOp[] = [];
  const record =
    (op: string) =>
    (...args: unknown[]): unknown => {
      ops.push({ op, args });
      return undefined;
    };
  const ctx = {
    set strokeStyle(_v: unknown) {},
    get strokeStyle(): unknown { return undefined; },
    set fillStyle(_v: unknown) {},
    get fillStyle(): unknown { return undefined; },
    set lineWidth(_v: unknown) {},
    get lineWidth(): unknown { return 1; },
    clearRect: record("clearRect"),
    fillRect: record("fillRect"),
    beginPath: record("beginPath"),
    moveTo: record("moveTo"),
    lineTo: record("lineTo"),
    stroke: record("stroke"),
    createLinearGradient: (...args: unknown[]): unknown => {
      ops.push({ op: "createLinearGradient", args });
      return { addColorStop: (): void => {} };
    },
  } as unknown as CanvasRenderingContext2D;
  const original = HTMLCanvasElement.prototype.getContext;
  Object.defineProperty(HTMLCanvasElement.prototype, "getContext", {
    configurable: true,
    value: function getContext(this: HTMLCanvasElement, type: string) {
      return type === "2d" ? ctx : null;
    },
  });
  return {
    ops,
    restore: (): void => {
      Object.defineProperty(HTMLCanvasElement.prototype, "getContext", {
        configurable: true,
        value: original,
      });
    },
  };
};

const makeTrack = (id: string): LibraryTrack => ({
  id,
  path: `/m/${id}.mp3`,
  bpm: 124.0,
  camelot_key: "8B",
  energy: 0.2,
  duration_s: 200.0,
  beat_grid_anchor_ms: 0,
  beat_period_ms: 60_000.0 / 124.0,
  downbeats_ms: [],
  hot_cues: [null, null, null, null, null, null, null, null],
});

// Tiny 4-pair peak buffer (8 bytes) — matches the Int8Array layout
// the waveform store hands back so the canvas draw path actually
// executes (covers the non-flat branch in compactMode).
const PEAKS_B64 = ((): string => {
  const bytes = new Uint8Array([200, 56, 220, 36, 180, 76, 230, 26]);
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
})();

type Call = (method: string, params?: unknown) => Promise<unknown>;

const makeClient = (peaksB64: string | null = PEAKS_B64): {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
} => {
  const call = vi.fn<Call>(
    (method: string, params?: unknown): Promise<unknown> => {
      if (method === "library.get_waveform") {
        const p = params as { track_id: string } | undefined;
        return Promise.resolve({
          track_id: p?.track_id ?? "",
          peaks_b64: peaksB64,
        });
      }
      return Promise.reject(new Error(`unmocked: ${method}`));
    },
  );
  return { client: { call } as unknown as JsonRpcWS, call };
};

describe("TrackRow hover preview", () => {
  let canvas: { restore: () => void; ops: CanvasOp[] };

  beforeEach((): void => {
    __resetTrackRowHoverCache();
    __resetWaveformCache();
    canvas = installCanvasRecorder();
  });
  afterEach((): void => {
    cleanup();
    canvas.restore();
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("hover after 200ms debounce triggers library.get_waveform", async (): Promise<void> => {
    vi.useFakeTimers();
    const track = makeTrack("alpha");
    const { client, call } = makeClient();
    render(<TrackRow track={track} client={client} />);

    const row = screen.getByTestId("track-row-alpha");
    fireEvent.mouseEnter(row);
    // Before debounce expires, no RPC has fired.
    expect(call).not.toHaveBeenCalled();

    await act(async (): Promise<void> => {
      vi.advanceTimersByTime(199);
    });
    expect(call).not.toHaveBeenCalled();

    await act(async (): Promise<void> => {
      vi.advanceTimersByTime(1);
    });
    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith("library.get_waveform", {
      track_id: "alpha",
    });

    // Preview container is mounted.
    expect(screen.getByTestId("track-row-preview-alpha")).toBeTruthy();
  });

  it("quick hover-leave before debounce doesn't fetch", async (): Promise<void> => {
    vi.useFakeTimers();
    const track = makeTrack("bravo");
    const { client, call } = makeClient();
    render(<TrackRow track={track} client={client} />);
    const row = screen.getByTestId("track-row-bravo");

    fireEvent.mouseEnter(row);
    await act(async (): Promise<void> => {
      vi.advanceTimersByTime(120);
    });
    fireEvent.mouseLeave(row);
    await act(async (): Promise<void> => {
      vi.advanceTimersByTime(500);
    });

    expect(call).not.toHaveBeenCalled();
    expect(screen.queryByTestId("track-row-preview-bravo")).toBeNull();
  });

  it("re-hover within 30s does not issue a second RPC", async (): Promise<void> => {
    const track = makeTrack("charlie");
    const { client, call } = makeClient();
    // No fake timers — let the 200ms debounce run on the real loop
    // so the test exercises the full hover -> fetch -> setState path
    // rather than the timer-flush corner case.
    render(
      <TrackRow
        track={track}
        client={client}
        hoverDebounceMs={0}
      />,
    );
    const row = screen.getByTestId("track-row-charlie");

    fireEvent.mouseEnter(row);
    await waitFor((): void => {
      expect(call).toHaveBeenCalledTimes(1);
    });
    fireEvent.mouseLeave(row);

    // Second hover within the 30s window — recency gate skips the RPC.
    fireEvent.mouseEnter(row);
    // Give the microtask queue a turn so any spurious .then would land.
    await new Promise((r): void => {
      setTimeout(r, 10);
    });
    expect(call).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("track-row-preview-charlie")).toBeTruthy();
  });

  it("re-hover after the window expires DOES refetch (gate is bounded)", async (): Promise<void> => {
    const track = makeTrack("delta");
    const { client, call } = makeClient();
    render(
      <TrackRow
        track={track}
        client={client}
        hoverDebounceMs={0}
        // Tiny window so we can exit it without a fake clock.
        rehoverCacheMs={5}
      />,
    );
    const row = screen.getByTestId("track-row-delta");

    fireEvent.mouseEnter(row);
    await waitFor((): void => {
      expect(call).toHaveBeenCalledTimes(1);
    });
    fireEvent.mouseLeave(row);

    // Drop the waveform cache too, otherwise fetchWaveform short-circuits
    // before reaching the RPC layer even when the recency gate has
    // expired. The recency gate skips the React-state churn; the
    // store-level cache is the durable resolution path. Test isolates
    // the gate logic by clearing the store cache.
    __resetWaveformCache();

    await new Promise((r): void => {
      setTimeout(r, 20);
    });
    fireEvent.mouseEnter(row);
    await waitFor((): void => {
      expect(call).toHaveBeenCalledTimes(2);
    });
  });

  it("long-press 500ms on mobile (touchstart) shows the preview", async (): Promise<void> => {
    vi.useFakeTimers();
    const track = makeTrack("echo");
    const { client, call } = makeClient();
    render(<TrackRow track={track} client={client} />);
    const row = screen.getByTestId("track-row-echo");

    // Synthetic single-finger touch.
    fireEvent.touchStart(row, {
      touches: [{ clientX: 0, clientY: 0 }],
    });
    await act(async (): Promise<void> => {
      vi.advanceTimersByTime(499);
    });
    expect(call).not.toHaveBeenCalled();

    await act(async (): Promise<void> => {
      vi.advanceTimersByTime(1);
    });
    expect(call).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("track-row-preview-echo")).toBeTruthy();

    // Touch end hides the preview.
    fireEvent.touchEnd(row);
    expect(screen.queryByTestId("track-row-preview-echo")).toBeNull();
  });

  it("compact-mode canvas is marked data-compact=true (no playhead, no bar.beat label)", async (): Promise<void> => {
    const track = makeTrack("foxtrot");
    const { client } = makeClient();
    render(
      <TrackRow
        track={track}
        client={client}
        hoverDebounceMs={0}
      />,
    );
    const row = screen.getByTestId("track-row-foxtrot");
    fireEvent.mouseEnter(row);

    await waitFor((): void => {
      expect(screen.getByTestId("track-row-preview-foxtrot")).toBeTruthy();
    });
    const canvasEl = screen
      .getByTestId("track-row-preview-foxtrot")
      .querySelector("canvas") as HTMLCanvasElement | null;
    expect(canvasEl).not.toBeNull();
    expect(canvasEl?.getAttribute("data-compact")).toBe("true");
    // Scroll-mode bar.beat label is suppressed in compactMode.
    expect(screen.queryByTestId("waveform-barbeat")).toBeNull();
  });

  it("mouseleave after preview opens hides the preview", async (): Promise<void> => {
    const track = makeTrack("golf");
    const { client } = makeClient();
    render(
      <TrackRow
        track={track}
        client={client}
        hoverDebounceMs={0}
      />,
    );
    const row = screen.getByTestId("track-row-golf");
    fireEvent.mouseEnter(row);
    await waitFor((): void => {
      expect(screen.getByTestId("track-row-preview-golf")).toBeTruthy();
    });
    fireEvent.mouseLeave(row);
    expect(screen.queryByTestId("track-row-preview-golf")).toBeNull();
  });

  it("does not render the pending-sync chip by default", (): void => {
    const track = makeTrack("hotel");
    const { client } = makeClient();
    render(<TrackRow track={track} client={client} />);
    expect(screen.queryByTestId("track-row-pending-hotel")).toBeNull();
  });

  it("renders the pending-sync chip when pendingSync=true", (): void => {
    const track = makeTrack("india");
    const { client } = makeClient();
    render(
      <TrackRow track={track} client={client} pendingSync />,
    );
    const chip = screen.getByTestId("track-row-pending-india");
    expect(chip.textContent).toBe("⟳ pending");
    expect(chip.getAttribute("aria-label")).toBe("awaiting cloud push");
  });
});
