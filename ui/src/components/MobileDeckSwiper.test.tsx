// MobileDeckSwiper + useViewport tests.
//
// Covers the responsive layout slice introduced for the
// mobile/tablet/desktop variants:
//   1. useViewport returns the right enum at each breakpoint.
//   2. Swipe left on Deck A switches to Deck B.
//   3. Swipe right on Deck B switches back to Deck A.
//   4. Tapping the second dot indicator jumps to Deck B.
//   5. Swipe shorter than threshold is ignored.
//   6. The non-active deck pane is hidden via aria-hidden.
//
// + DeckRow integration tests (paged on mobile, drawer toggling).
//
// jsdom doesn't fire `resize` automatically when you mutate
// `window.innerWidth`, so we set the width then dispatch the event
// manually before re-asserting.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, renderHook, screen } from "@testing-library/react";
import { fireEvent } from "@testing-library/react";
import { MobileDeckSwiper } from "./MobileDeckSwiper";
import { breakpointFor, useViewport } from "../hooks/useViewport";
import type { Deck as DeckState } from "../store/engine";
import type { JsonRpcWS } from "../ws/client";

const makeClient = (): JsonRpcWS =>
  ({
    call: vi.fn().mockResolvedValue(undefined),
  }) as unknown as JsonRpcWS;

const emptySlot = (): DeckState["effects"][number] => ({
  effect_id: 0,
  params: {},
  wet_dry: 0.5,
  enabled: false,
});

const baseDeck = (id: "A" | "B"): DeckState => ({
  id,
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
  effects: [emptySlot(), emptySlot(), emptySlot()],
  stem_gains: [1, 1, 1, 1],
  stem_mode: false,
});

/** Reset window.innerWidth to a deterministic value between tests so a
 * prior test's mutation doesn't leak. */
const setInnerWidth = (w: number): void => {
  Object.defineProperty(window, "innerWidth", {
    configurable: true,
    writable: true,
    value: w,
  });
  window.dispatchEvent(new Event("resize"));
};

describe("useViewport / breakpointFor", () => {
  beforeEach((): void => {
    setInnerWidth(1024);
  });
  afterEach((): void => {
    cleanup();
    setInnerWidth(1024);
  });

  it("breakpointFor returns 'mobile' for < 768 px widths", (): void => {
    expect(breakpointFor(320)).toBe("mobile");
    expect(breakpointFor(360)).toBe("mobile");
    expect(breakpointFor(767)).toBe("mobile");
  });

  it("breakpointFor returns 'tablet' between 768 and 1023 px", (): void => {
    expect(breakpointFor(768)).toBe("tablet");
    expect(breakpointFor(900)).toBe("tablet");
    expect(breakpointFor(1023)).toBe("tablet");
  });

  it("breakpointFor returns 'desktop' at >= 1024 px", (): void => {
    expect(breakpointFor(1024)).toBe("desktop");
    expect(breakpointFor(1920)).toBe("desktop");
  });

  it("useViewport tracks window resize events", (): void => {
    setInnerWidth(1280);
    const { result } = renderHook((): ReturnType<typeof useViewport> =>
      useViewport(),
    );
    expect(result.current).toBe("desktop");

    act((): void => {
      setInnerWidth(800);
    });
    expect(result.current).toBe("tablet");

    act((): void => {
      setInnerWidth(360);
    });
    expect(result.current).toBe("mobile");
  });
});

describe("MobileDeckSwiper", () => {
  afterEach((): void => {
    cleanup();
  });

  const swipe = (
    el: HTMLElement,
    startX: number,
    endX: number,
  ): void => {
    fireEvent.touchStart(el, {
      touches: [{ clientX: startX, clientY: 0 }],
    });
    fireEvent.touchMove(el, {
      touches: [{ clientX: endX, clientY: 0 }],
    });
    fireEvent.touchEnd(el, { changedTouches: [{ clientX: endX, clientY: 0 }] });
  };

  it("renders Deck A as visible and Deck B as hidden by default", (): void => {
    render(
      <MobileDeckSwiper
        decks={[baseDeck("A"), baseDeck("B")]}
        client={makeClient()}
      />,
    );
    const paneA = screen.getByTestId("mobile-deck-pane-A");
    const paneB = screen.getByTestId("mobile-deck-pane-B");
    expect(paneA.getAttribute("aria-hidden")).toBe("false");
    expect(paneB.getAttribute("aria-hidden")).toBe("true");
  });

  it("swipe left past threshold switches from Deck A to Deck B", (): void => {
    render(
      <MobileDeckSwiper
        decks={[baseDeck("A"), baseDeck("B")]}
        client={makeClient()}
      />,
    );
    const wrap = screen.getByTestId("mobile-deck-swiper");
    swipe(wrap, 200, 50); // dx = -150 > 80 threshold
    expect(screen.getByTestId("mobile-deck-pane-A").getAttribute("aria-hidden"))
      .toBe("true");
    expect(screen.getByTestId("mobile-deck-pane-B").getAttribute("aria-hidden"))
      .toBe("false");
  });

  it("swipe shorter than threshold is ignored", (): void => {
    render(
      <MobileDeckSwiper
        decks={[baseDeck("A"), baseDeck("B")]}
        client={makeClient()}
      />,
    );
    const wrap = screen.getByTestId("mobile-deck-swiper");
    swipe(wrap, 200, 180); // dx = -20 < 80 threshold
    expect(screen.getByTestId("mobile-deck-pane-A").getAttribute("aria-hidden"))
      .toBe("false");
  });

  it("dot indicator updates when deck changes", (): void => {
    render(
      <MobileDeckSwiper
        decks={[baseDeck("A"), baseDeck("B")]}
        client={makeClient()}
      />,
    );
    const dotA = screen.getByTestId("mobile-deck-dot-A");
    const dotB = screen.getByTestId("mobile-deck-dot-B");
    expect(dotA.getAttribute("aria-selected")).toBe("true");
    expect(dotB.getAttribute("aria-selected")).toBe("false");

    fireEvent.click(dotB);
    expect(dotA.getAttribute("aria-selected")).toBe("false");
    expect(dotB.getAttribute("aria-selected")).toBe("true");
  });

  it("swipe right on Deck B returns to Deck A", (): void => {
    render(
      <MobileDeckSwiper
        decks={[baseDeck("A"), baseDeck("B")]}
        client={makeClient()}
      />,
    );
    // Jump to deck B via dot click first.
    fireEvent.click(screen.getByTestId("mobile-deck-dot-B"));
    const wrap = screen.getByTestId("mobile-deck-swiper");
    swipe(wrap, 50, 200); // dx = +150 > 80
    expect(screen.getByTestId("mobile-deck-pane-A").getAttribute("aria-hidden"))
      .toBe("false");
    expect(screen.getByTestId("mobile-deck-pane-B").getAttribute("aria-hidden"))
      .toBe("true");
  });

  it("custom swipeThresholdPx=0 fires on any movement", (): void => {
    render(
      <MobileDeckSwiper
        decks={[baseDeck("A"), baseDeck("B")]}
        client={makeClient()}
        swipeThresholdPx={0}
      />,
    );
    const wrap = screen.getByTestId("mobile-deck-swiper");
    // Even tiny dx must trigger because threshold is 0 — but Math.abs(0)
    // is still not less than 0, so we need a non-zero movement.
    swipe(wrap, 100, 99);
    expect(screen.getByTestId("mobile-deck-pane-B").getAttribute("aria-hidden"))
      .toBe("false");
  });

  it("swipe left on Deck B clamps (no wrap-around)", (): void => {
    render(
      <MobileDeckSwiper
        decks={[baseDeck("A"), baseDeck("B")]}
        client={makeClient()}
      />,
    );
    fireEvent.click(screen.getByTestId("mobile-deck-dot-B"));
    const wrap = screen.getByTestId("mobile-deck-swiper");
    swipe(wrap, 200, 50); // dx = -150, but already at B → no change
    expect(screen.getByTestId("mobile-deck-pane-B").getAttribute("aria-hidden"))
      .toBe("false");
  });
});
