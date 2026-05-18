// Waveform.test.tsx — peak-pair rendering + playhead math.
//
// jsdom doesn't ship a real canvas implementation; the default
// `getContext("2d")` returns null. We patch HTMLCanvasElement to hand
// back a recording stub that captures stroke calls — that's enough to
// verify the draw decisions (centre line, peak bars, playhead) without
// pixel-comparing.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import {
  WAVEFORM_GRADIENT_BOTTOM,
  WAVEFORM_GRADIENT_TOP,
  Waveform,
  peakToY,
  playheadX,
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
      render(<Waveform peaks={null} width={100} height={40} />);
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
        <Waveform peaks={peaks} width={16} height={32} />,
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
      render(<Waveform peaks={peaks} width={20} height={20} />);
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
  });
});
