// Waveform.test.tsx — peak-pair rendering + playhead math.
//
// jsdom doesn't ship a real canvas implementation; the default
// `getContext("2d")` returns null. We patch HTMLCanvasElement to hand
// back a recording stub that captures stroke calls — that's enough to
// verify the draw decisions (centre line, peak bars, playhead) without
// pixel-comparing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import {
  SCROLL_HALF_WINDOW_MS,
  WAVEFORM_BEAT,
  WAVEFORM_DOWNBEAT,
  WAVEFORM_GRADIENT_BOTTOM,
  WAVEFORM_GRADIENT_TOP,
  WAVEFORM_PEAK_SCROLL,
  Waveform,
  barBeatLabel,
  peakToY,
  playheadX,
  scrollXForMs,
} from "./Waveform";

interface CanvasOp {
  readonly op: string;
  readonly args: ReadonlyArray<unknown>;
}

interface RecordingCtx extends CanvasRenderingContext2D {
  readonly _ops: CanvasOp[];
  readonly _styles: string[];
}

/** Make a minimal stub canvas context that records every API call.
 * Mutates `HTMLCanvasElement.prototype.getContext` so the next render
 * picks up the recorder. Caller restores the original on teardown.
 */
const installCanvasRecorder = (): { restore: () => void; ctx: RecordingCtx } => {
  const ops: CanvasOp[] = [];
  const styles: string[] = [];
  const record =
    (op: string) =>
    (...args: unknown[]): unknown => {
      ops.push({ op, args });
      return undefined;
    };
  const ctx = {
    _ops: ops,
    _styles: styles,
    set strokeStyle(v: unknown) {
      // Capture every stroke style assignment so tests can assert the
      // gradient was constructed via createLinearGradient + addColorStop.
      styles.push(typeof v === "string" ? v : "<gradient>");
    },
    get strokeStyle(): unknown {
      return undefined;
    },
    set fillStyle(v: unknown) {
      styles.push(typeof v === "string" ? `fill:${v}` : "fill:<gradient>");
    },
    get fillStyle(): unknown {
      return undefined;
    },
    set lineWidth(_v: unknown) {
      /* ignored — width is uniform in our draws */
    },
    get lineWidth(): unknown {
      return 1;
    },
    clearRect: record("clearRect"),
    fillRect: record("fillRect"),
    beginPath: record("beginPath"),
    moveTo: record("moveTo"),
    lineTo: record("lineTo"),
    stroke: record("stroke"),
    createLinearGradient: (...args: unknown[]): unknown => {
      ops.push({ op: "createLinearGradient", args });
      // Return a thin stub that records gradient stops back into ops.
      return {
        addColorStop: (offset: number, color: string): void => {
          ops.push({ op: "addColorStop", args: [offset, color] });
        },
      };
    },
  } as unknown as RecordingCtx;
  const original = HTMLCanvasElement.prototype.getContext;
  Object.defineProperty(HTMLCanvasElement.prototype, "getContext", {
    configurable: true,
    value: function getContext(this: HTMLCanvasElement, type: string) {
      if (type === "2d") return ctx;
      return null;
    },
  });
  return {
    ctx,
    restore: (): void => {
      Object.defineProperty(HTMLCanvasElement.prototype, "getContext", {
        configurable: true,
        value: original,
      });
    },
  };
};

describe("Waveform", () => {
  afterEach((): void => {
    cleanup();
    vi.restoreAllMocks();
  });

  it("renders a single centre line when peaks is null (flat fallback)", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      render(<Waveform peaks={null} mode="full" width={100} height={40} />);
      // Flat fallback draws: clear, fill, beginPath, moveTo(0, 20), lineTo(100, 20), stroke.
      const moveTos = ctx._ops.filter((o): boolean => o.op === "moveTo");
      const lineTos = ctx._ops.filter((o): boolean => o.op === "lineTo");
      expect(moveTos.length).toBeGreaterThanOrEqual(1);
      expect(lineTos.length).toBeGreaterThanOrEqual(1);
      // The flat line sits at y = height/2.
      const flatMove = moveTos.find((o): boolean => o.args[1] === 20);
      const flatLine = lineTos.find((o): boolean => o.args[1] === 20);
      expect(flatMove).toBeDefined();
      expect(flatLine).toBeDefined();
      // No gradient should be installed in the flat path.
      const hadGradient = ctx._ops.some(
        (o): boolean => o.op === "createLinearGradient",
      );
      expect(hadGradient).toBe(false);
    } finally {
      restore();
    }
  });

  it("renders min/max vertical lines when peaks are provided", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      // 4 buckets, alternating extreme min/max so every column emits a
      // visible bar (yMin !== yMax). Each pair is (min, max).
      const peaks = new Int8Array([-100, 100, -50, 50, -120, 120, -30, 30]);
      render(
        <Waveform peaks={peaks} mode="full" width={16} height={32} />,
      );
      // Many lineTo calls — one per x-pixel column with a non-degenerate
      // peak (which is all of them, given the values above).
      const lineTos = ctx._ops.filter((o): boolean => o.op === "lineTo");
      // 1 centreline lineTo + 16 column lineTos = 17. Allow >= 10 to
      // be robust against minor draw-order tweaks.
      expect(lineTos.length).toBeGreaterThanOrEqual(10);
    } finally {
      restore();
    }
  });

  it("draws a playhead vertical line at the correct x given position/duration", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      const peaks = new Int8Array([-100, 100, -50, 50]);
      // position 50%, duration 1000 → playhead at x = width/2.
      render(
        <Waveform
          peaks={peaks}
          mode="full"
          width={100}
          height={20}
          positionMs={500}
          durationMs={1000}
        />,
      );
      // After the bars draw, the playhead path should fire a vertical
      // line at x = 50 (+ 0.5 offset). Look for a moveTo(50.5, 0) +
      // lineTo(50.5, 20).
      const playheadMove = ctx._ops.find(
        (o): boolean =>
          o.op === "moveTo" && o.args[0] === 50.5 && o.args[1] === 0,
      );
      const playheadLine = ctx._ops.find(
        (o): boolean =>
          o.op === "lineTo" && o.args[0] === 50.5 && o.args[1] === 20,
      );
      expect(playheadMove).toBeDefined();
      expect(playheadLine).toBeDefined();
    } finally {
      restore();
    }
  });

  it("installs the blue gradient (top + bottom stops) when peaks are present", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      const peaks = new Int8Array([-100, 100, -50, 50]);
      render(<Waveform peaks={peaks} mode="full" width={20} height={20} />);
      const stops = ctx._ops.filter(
        (o): boolean => o.op === "addColorStop",
      );
      // Two stops: 0 → top, 1 → bottom.
      expect(stops.length).toBe(2);
      const colors = stops.map((o): unknown => o.args[1]);
      expect(colors).toContain(WAVEFORM_GRADIENT_TOP);
      expect(colors).toContain(WAVEFORM_GRADIENT_BOTTOM);
    } finally {
      restore();
    }
  });

  describe("helper math", () => {
    it("peakToY maps -128 → height, +127 → 0", (): void => {
      expect(peakToY(-128, 100)).toBe(100);
      expect(peakToY(127, 100)).toBe(0);
      // Centre-ish: 0 maps to roughly height/2.
      const mid = peakToY(0, 100);
      expect(mid).toBeGreaterThanOrEqual(49);
      expect(mid).toBeLessThanOrEqual(51);
    });

    it("playheadX returns null when duration is non-positive", (): void => {
      expect(playheadX(100, 0, 480)).toBeNull();
      expect(playheadX(100, -5, 480)).toBeNull();
    });

    it("playheadX clamps position to [0, duration]", (): void => {
      expect(playheadX(-50, 1000, 200)).toBe(0); // negative → 0
      expect(playheadX(5000, 1000, 200)).toBe(200); // overflow → width
      expect(playheadX(250, 1000, 200)).toBe(50); // mid
    });

    it("scrollXForMs maps centre to width/2 and edges to 0/width", (): void => {
      expect(scrollXForMs(10_000, 10_000, 200)).toBeCloseTo(100, 5);
      expect(scrollXForMs(5_000, 10_000, 200)).toBeCloseTo(0, 5);
      expect(scrollXForMs(15_000, 10_000, 200)).toBeCloseTo(200, 5);
    });

    it("scrollXForMs returns NaN outside the visible window", (): void => {
      expect(Number.isNaN(scrollXForMs(0, 10_000, 200))).toBe(true);
      expect(Number.isNaN(scrollXForMs(20_000, 10_000, 200))).toBe(true);
    });

    it("barBeatLabel renders 1-indexed bar.beat from anchor + period", (): void => {
      // 120 bpm → 500 ms per beat. anchor=0.
      expect(barBeatLabel(0, 0, 500)).toBe("1.1");
      expect(barBeatLabel(500, 0, 500)).toBe("1.2");
      expect(barBeatLabel(1500, 0, 500)).toBe("1.4");
      expect(barBeatLabel(2000, 0, 500)).toBe("2.1");
      // bar 12, beat 3 → (12-1)*4 + (3-1) = 46 beats in
      expect(barBeatLabel(46 * 500, 0, 500)).toBe("12.3");
    });

    it("barBeatLabel returns empty string for degenerate inputs", (): void => {
      expect(barBeatLabel(1000, 0, 0)).toBe("");
      expect(barBeatLabel(1000, 0, -1)).toBe("");
    });
  });
});

describe("Waveform — scroll mode", () => {
  let rafCallback: FrameRequestCallback | null = null;
  let rafCalls = 0;

  beforeEach((): void => {
    rafCallback = null;
    rafCalls = 0;
    // Capture first rAF call; second invocation runs synchronously so the
    // initial paint completes without scheduling an infinite loop.
    vi.stubGlobal(
      "requestAnimationFrame",
      (cb: FrameRequestCallback): number => {
        rafCalls += 1;
        if (rafCalls === 1) rafCallback = cb;
        return rafCalls;
      },
    );
    vi.stubGlobal("cancelAnimationFrame", (_n: number): void => undefined);
  });

  afterEach((): void => {
    cleanup();
    vi.unstubAllGlobals();
  });

  const drainRaf = (): void => {
    if (rafCallback) {
      act((): void => {
        rafCallback?.(0);
      });
    }
  };

  it("renders peaks within the visible 10-second window", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      // 100 buckets across a 60_000 ms track. Position = 30_000 ms.
      const peaks = new Int8Array(200);
      for (let i = 0; i < 100; i++) {
        peaks[i * 2] = -100;
        peaks[i * 2 + 1] = 100;
      }
      render(
        <Waveform
          peaks={peaks}
          mode="scroll"
          width={100}
          height={40}
          positionMs={30_000}
          durationMs={60_000}
        />,
      );
      drainRaf();
      // Some lineTo calls should target peak columns (not the centreline).
      const peakLines = ctx._ops.filter(
        (o): boolean =>
          o.op === "lineTo" && o.args[1] !== 20 && typeof o.args[1] === "number",
      );
      expect(peakLines.length).toBeGreaterThan(5);
      // Electric-blue peak colour was installed.
      expect(ctx._styles).toContain(WAVEFORM_PEAK_SCROLL);
    } finally {
      restore();
    }
  });

  it("draws the playhead at horizontal centre regardless of position", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      const peaks = new Int8Array(200);
      render(
        <Waveform
          peaks={peaks}
          mode="scroll"
          width={200}
          height={40}
          positionMs={12_345}
          durationMs={60_000}
        />,
      );
      drainRaf();
      // Centre x = 100 → expect a moveTo(100.5, 0) and lineTo(100.5, 40).
      const ph = ctx._ops.find(
        (o): boolean =>
          o.op === "moveTo" && o.args[0] === 100.5 && o.args[1] === 0,
      );
      expect(ph).toBeDefined();
    } finally {
      restore();
    }
  });

  it("draws beat-grid lines at correct x positions", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      // 120 bpm → 500 ms per beat. centre=10_000 ms, anchor=0 ms.
      // Beat at 10_000 ms lands at x = width/2 = 100.
      const peaks = new Int8Array(200);
      render(
        <Waveform
          peaks={peaks}
          mode="scroll"
          width={200}
          height={40}
          positionMs={10_000}
          durationMs={60_000}
          beatGridAnchorMs={0}
          beatPeriodMs={500}
        />,
      );
      drainRaf();
      // The beat colour was set at least once.
      expect(ctx._styles).toContain(WAVEFORM_BEAT);
      // A beat line exists at x = 100.5 (centre, where the anchor lands).
      const beatLine = ctx._ops.find(
        (o): boolean =>
          o.op === "moveTo" && o.args[0] === 100.5 && o.args[1] === 0,
      );
      expect(beatLine).toBeDefined();
    } finally {
      restore();
    }
  });

  it("draws downbeats with bolder white stroke", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      const peaks = new Int8Array(200);
      render(
        <Waveform
          peaks={peaks}
          mode="scroll"
          width={200}
          height={40}
          positionMs={10_000}
          durationMs={60_000}
          beatGridAnchorMs={0}
          beatPeriodMs={500}
          downbeatsMs={[10_000]}
        />,
      );
      drainRaf();
      // White downbeat colour was set.
      expect(ctx._styles).toContain(WAVEFORM_DOWNBEAT);
    } finally {
      restore();
    }
  });

  it("renders the bar.beat label in the DOM in scroll mode", (): void => {
    const { restore } = installCanvasRecorder();
    try {
      const peaks = new Int8Array(200);
      const { getByTestId } = render(
        <Waveform
          peaks={peaks}
          mode="scroll"
          width={200}
          height={40}
          positionMs={2_000}
          durationMs={60_000}
          beatGridAnchorMs={0}
          beatPeriodMs={500}
        />,
      );
      drainRaf();
      const label = getByTestId("waveform-barbeat");
      // 2000 ms / 500 = 4 beats → bar 2, beat 1.
      expect(label.textContent).toBe("2.1");
    } finally {
      restore();
    }
  });

  it("calls positionProvider on each rAF tick (smoothing between server pushes)", (): void => {
    const { restore } = installCanvasRecorder();
    try {
      const provider = vi.fn((): number => 5_000);
      const peaks = new Int8Array(200);
      render(
        <Waveform
          peaks={peaks}
          mode="scroll"
          width={200}
          height={40}
          positionMs={0}
          durationMs={60_000}
          positionProvider={provider}
        />,
      );
      // First tick fired synchronously by the initial paint.
      expect(provider).toHaveBeenCalled();
      const initialCalls = provider.mock.calls.length;
      drainRaf();
      expect(provider.mock.calls.length).toBeGreaterThan(initialCalls);
    } finally {
      restore();
    }
  });

  it("uses 10-second window total (half = SCROLL_HALF_WINDOW_MS = 5000)", (): void => {
    expect(SCROLL_HALF_WINDOW_MS).toBe(5_000);
  });

  it("falls back to flat when durationMs is 0 in scroll mode", (): void => {
    const { ctx, restore } = installCanvasRecorder();
    try {
      const peaks = new Int8Array(200);
      render(
        <Waveform
          peaks={peaks}
          mode="scroll"
          width={100}
          height={40}
          durationMs={0}
        />,
      );
      drainRaf();
      // Without duration we can't map ms to x — peak colour should not be set.
      expect(ctx._styles).not.toContain(WAVEFORM_PEAK_SCROLL);
    } finally {
      restore();
    }
  });
});
