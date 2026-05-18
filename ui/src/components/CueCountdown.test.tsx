// CueCountdown.test.tsx — verifies the pro-DJ next-downbeat readout.
//
// Coverage:
//   1. Pure helpers (msToNextDownbeat / currentBarIndex / countdownDigit /
//      formatSecondsReadout) — fast unit tests, no DOM.
//   2. Component render — rAF stubbed so we can drain one tick
//      deterministically (same pattern as Waveform.test.tsx).
//
// The component uses direct DOM mutation in its rAF loop (60 Hz state
// updates would burn React reconciles), so we assert against the
// post-tick `textContent` and `data-state` attribute, not the initial
// JSX render.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";
import {
  CueCountdown,
  COUNTDOWN_THRESHOLD_MS,
  BAR_FLASH_WINDOW_MS,
  countdownDigit,
  currentBarIndex,
  formatSecondsReadout,
  msToNextDownbeat,
} from "./CueCountdown";

describe("CueCountdown — pure helpers", () => {
  it("msToNextDownbeat returns the gap to the next future downbeat", (): void => {
    const beats = [0, 2_000, 4_000, 6_000];
    expect(msToNextDownbeat(0, beats)).toBe(0);
    expect(msToNextDownbeat(500, beats)).toBe(1_500);
    expect(msToNextDownbeat(1_999, beats)).toBe(1);
    expect(msToNextDownbeat(2_000, beats)).toBe(0);
    expect(msToNextDownbeat(5_999, beats)).toBe(1);
    // No future downbeat → null (end of track).
    expect(msToNextDownbeat(7_000, beats)).toBeNull();
    expect(msToNextDownbeat(100, [])).toBeNull();
  });

  it("currentBarIndex counts downbeats strictly at-or-before position", (): void => {
    const beats = [0, 2_000, 4_000, 6_000, 8_000];
    expect(currentBarIndex(-1, beats)).toBe(0);
    expect(currentBarIndex(0, beats)).toBe(1);
    expect(currentBarIndex(2_500, beats)).toBe(2);
    expect(currentBarIndex(8_000, beats)).toBe(5);
    expect(currentBarIndex(9_999, beats)).toBe(5);
    expect(currentBarIndex(500, [])).toBe(0);
  });

  it("countdownDigit maps ms-remaining → big-digit display", (): void => {
    expect(countdownDigit(2_000)).toBe("3");
    expect(countdownDigit(1_999)).toBe("3");
    expect(countdownDigit(1_500)).toBe("2");
    expect(countdownDigit(1_499)).toBe("2");
    expect(countdownDigit(1_000)).toBe("1");
    expect(countdownDigit(999)).toBe("1");
    expect(countdownDigit(500)).toBe("0");
    expect(countdownDigit(0)).toBe("0");
  });

  it("formatSecondsReadout formats >2 s readout with one decimal", (): void => {
    expect(formatSecondsReadout(3_200)).toBe("Next bar in 3.2s");
    expect(formatSecondsReadout(5_150)).toBe("Next bar in 5.2s");
    expect(formatSecondsReadout(10_000)).toBe("Next bar in 10.0s");
  });

  it("exports sensible threshold + flash-window constants", (): void => {
    expect(COUNTDOWN_THRESHOLD_MS).toBe(2_000);
    expect(BAR_FLASH_WINDOW_MS).toBeGreaterThan(0);
    expect(BAR_FLASH_WINDOW_MS).toBeLessThan(1_000);
  });
});

describe("CueCountdown — component render", () => {
  let rafCallback: FrameRequestCallback | null = null;
  let rafCalls = 0;
  let cancelCount = 0;

  beforeEach((): void => {
    rafCallback = null;
    rafCalls = 0;
    cancelCount = 0;
    // Capture first rAF; subsequent rAFs are stored over but don't auto-
    // fire — drainRaf invokes them deterministically.
    vi.stubGlobal(
      "requestAnimationFrame",
      (cb: FrameRequestCallback): number => {
        rafCalls += 1;
        rafCallback = cb;
        return rafCalls;
      },
    );
    vi.stubGlobal("cancelAnimationFrame", (_n: number): void => {
      cancelCount += 1;
    });
  });

  afterEach((): void => {
    cleanup();
    vi.unstubAllGlobals();
  });

  const drainRaf = (times = 1): void => {
    for (let i = 0; i < times; i++) {
      const cb = rafCallback;
      if (!cb) return;
      rafCallback = null;
      act((): void => {
        cb(0);
      });
    }
  };

  it("renders 'Next bar in Xs' format when next downbeat is >2 s away", (): void => {
    // Position 0, next downbeat at 4000 ms → 4.0 s away → calm readout.
    render(
      <CueCountdown
        deck="A"
        downbeatsMs={[0, 4_000, 8_000, 12_000]}
        beatPeriodMs={500}
        positionProvider={(): number => 0.001} // just past first downbeat
      />,
    );
    drainRaf();
    const readout = screen.getByTestId("cue-countdown-A-readout");
    expect(readout.textContent).toBe("Next bar in 4.0s");
    const wrap = screen.getByTestId("cue-countdown-A");
    expect(wrap.getAttribute("data-state")).toBe("seconds");
  });

  it("renders big countdown digit when next downbeat is <2 s away", (): void => {
    // Position 7_200, next downbeat at 8_000 → 800 ms remaining → digit "1".
    render(
      <CueCountdown
        deck="A"
        downbeatsMs={[0, 4_000, 8_000, 12_000]}
        beatPeriodMs={500}
        positionProvider={(): number => 7_200}
      />,
    );
    drainRaf();
    const readout = screen.getByTestId("cue-countdown-A-readout");
    expect(readout.textContent).toBe("1");
    const wrap = screen.getByTestId("cue-countdown-A");
    expect(wrap.getAttribute("data-state")).toBe("countdown");
    expect(wrap.getAttribute("data-digit")).toBe("1");
  });

  it("renders digit '0' when within 500 ms of the downbeat", (): void => {
    // Position 7_700, next downbeat at 8_000 → 300 ms → "0".
    render(
      <CueCountdown
        deck="B"
        downbeatsMs={[0, 4_000, 8_000]}
        beatPeriodMs={500}
        positionProvider={(): number => 7_700}
      />,
    );
    drainRaf();
    expect(screen.getByTestId("cue-countdown-B-readout").textContent).toBe("0");
  });

  it("flashes 'BAR' on the rAF tick that crosses a downbeat", (): void => {
    // Mock position so first tick is pre-downbeat, second tick is post.
    let pos = 3_900;
    render(
      <CueCountdown
        deck="A"
        downbeatsMs={[0, 4_000, 8_000]}
        beatPeriodMs={500}
        positionProvider={(): number => pos}
      />,
    );
    // Tick 1: pos = 3_900 → bar index = 1 (after first downbeat at 0).
    // The component initialises `lastBarIndex = -1`, so the first tick
    // doesn't flash (we'd flash on mount otherwise).
    drainRaf();
    // Tick 2: advance past 4_000 → bar index = 2, crossing → BAR flash.
    pos = 4_010;
    drainRaf();
    const readout = screen.getByTestId("cue-countdown-A-readout");
    expect(readout.textContent).toBe("BAR");
    const wrap = screen.getByTestId("cue-countdown-A");
    expect(wrap.getAttribute("data-state")).toBe("bar-flash");
  });

  it("phrase indicator computes (bar - 1) mod phraseBars correctly", (): void => {
    // Build 32 downbeats spaced 2 s. Position lands on bar #5 (=
    // index 4 in 0-based downbeat array, but currentBarIndex returns 5
    // because it counts ≤-pos hits).
    const beats: number[] = [];
    for (let i = 0; i < 32; i++) beats.push(i * 2_000);
    // pos = 4 * 2000 + 100 = 8_100 → 5 downbeats at-or-before → bar 5.
    // (bar - 1) mod 16 = 4 → "Bar 5 of 16".
    render(
      <CueCountdown
        deck="A"
        downbeatsMs={beats}
        beatPeriodMs={500}
        positionProvider={(): number => 8_100}
        phraseBars={16}
      />,
    );
    drainRaf();
    expect(screen.getByTestId("cue-countdown-A-phrase").textContent).toBe(
      "Bar 5 of 16",
    );
  });

  it("phrase indicator wraps modulo phraseBars at the 17th bar", (): void => {
    const beats: number[] = [];
    for (let i = 0; i < 32; i++) beats.push(i * 2_000);
    // pos at bar 17 → (17 - 1) mod 16 = 0 → "Bar 1 of 16" (phrase reset).
    render(
      <CueCountdown
        deck="A"
        downbeatsMs={beats}
        beatPeriodMs={500}
        positionProvider={(): number => 16 * 2_000 + 100}
        phraseBars={16}
      />,
    );
    drainRaf();
    expect(screen.getByTestId("cue-countdown-A-phrase").textContent).toBe(
      "Bar 1 of 16",
    );
  });

  it("renders the idle dash when no downbeats are provided", (): void => {
    render(
      <CueCountdown
        deck="A"
        downbeatsMs={[]}
        beatPeriodMs={0}
        positionProvider={(): number => 0}
      />,
    );
    drainRaf();
    expect(screen.getByTestId("cue-countdown-A-readout").textContent).toBe("—");
    expect(screen.getByTestId("cue-countdown-A").getAttribute("data-state")).toBe(
      "idle",
    );
  });

  it("cancels its rAF handle on unmount", (): void => {
    const { unmount } = render(
      <CueCountdown
        deck="A"
        downbeatsMs={[0, 4_000]}
        beatPeriodMs={500}
        positionProvider={(): number => 0}
      />,
    );
    drainRaf();
    expect(rafCalls).toBeGreaterThan(0);
    unmount();
    // The effect cleanup must invoke cancelAnimationFrame exactly once.
    expect(cancelCount).toBeGreaterThanOrEqual(1);
  });

  it("exposes role=status and an aria-label naming the deck", (): void => {
    render(
      <CueCountdown
        deck="B"
        downbeatsMs={[0, 4_000]}
        beatPeriodMs={500}
        positionProvider={(): number => 0}
      />,
    );
    const wrap = screen.getByTestId("cue-countdown-B");
    expect(wrap.getAttribute("role")).toBe("status");
    expect(wrap.getAttribute("aria-label")).toContain("deck B");
  });
});
