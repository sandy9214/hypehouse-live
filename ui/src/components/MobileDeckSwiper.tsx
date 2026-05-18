// MobileDeckSwiper — single-deck-at-a-time view for < 768 px viewports.
//
// On mobile we can't fit both decks side-by-side without each control
// shrinking below the 44 px Apple/Material tap-target floor. Instead we
// render one deck full-width and let the user swipe horizontally to
// switch between Deck A and Deck B. A two-dot indicator at the top
// shows which deck is currently visible so the touch affordance is
// immediately legible.
//
// Threshold: 80 px of horizontal travel triggers a switch. Anything
// shorter is treated as scroll / accidental drag (the decks themselves
// have a lot of vertical content that the user needs to pan through).
//
// The component is intentionally dumb — it owns the *which deck is
// visible* state + the swipe gesture, nothing else. Both decks are
// rendered (mounted) so play/cue state survives a swap; we toggle
// visibility via `display: none` rather than conditional rendering.

import { useRef, useState } from "react";
import type { CSSProperties, JSX, TouchEvent as ReactTouchEvent } from "react";
import { Deck } from "./Deck";
import type { Deck as DeckState } from "../store/engine";
import type { JsonRpcWS } from "../ws/client";

export interface MobileDeckSwiperProps {
  decks: readonly [DeckState, DeckState];
  client: JsonRpcWS;
  /** Override swipe threshold (px). Tests pass `0` for deterministic
   * single-pixel-flick switching. Default 80 px matches iOS pager UX. */
  swipeThresholdPx?: number;
}

const DEFAULT_THRESHOLD_PX = 80;

const wrapStyle: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  flex: 1,
  minHeight: 0,
  touchAction: "pan-y",
};

const dotsRowStyle: CSSProperties = {
  display: "flex",
  justifyContent: "center",
  alignItems: "center",
  gap: 12,
  padding: "8px 0",
  background: "#000",
  borderBottom: "1px solid #222",
};

const dotStyle = (active: boolean): CSSProperties => ({
  width: 10,
  height: 10,
  borderRadius: "50%",
  background: active ? "#cce0ff" : "#333",
  border: "1px solid #2c4361",
  transition: "background 0.15s",
});

const deckWrapStyle = (visible: boolean): CSSProperties => ({
  flex: 1,
  minHeight: 0,
  display: visible ? "flex" : "none",
  flexDirection: "column",
});

export const MobileDeckSwiper = ({
  decks,
  client,
  swipeThresholdPx = DEFAULT_THRESHOLD_PX,
}: MobileDeckSwiperProps): JSX.Element => {
  // Which deck is currently visible: 0 = A, 1 = B.
  const [index, setIndex] = useState<0 | 1>(0);

  // Touch start X — captured on touchstart, read on touchend. Stored
  // in a ref so re-renders during the gesture don't kick us into a
  // stale-closure trap.
  const startXRef = useRef<number | null>(null);
  const deltaXRef = useRef<number>(0);

  const onTouchStart = (e: ReactTouchEvent<HTMLDivElement>): void => {
    if (e.touches.length !== 1) {
      startXRef.current = null;
      return;
    }
    startXRef.current = e.touches[0].clientX;
    deltaXRef.current = 0;
  };

  const onTouchMove = (e: ReactTouchEvent<HTMLDivElement>): void => {
    if (startXRef.current === null || e.touches.length !== 1) return;
    deltaXRef.current = e.touches[0].clientX - startXRef.current;
  };

  const onTouchEnd = (): void => {
    const start = startXRef.current;
    startXRef.current = null;
    const dx = deltaXRef.current;
    deltaXRef.current = 0;
    if (start === null) return;
    if (Math.abs(dx) < swipeThresholdPx) return;
    // Swipe LEFT (negative dx) -> go to next deck. Swipe RIGHT (positive
    // dx) -> go to previous. Clamp at the edges; we have exactly 2 decks.
    if (dx < 0 && index === 0) setIndex(1);
    else if (dx > 0 && index === 1) setIndex(0);
  };

  const goTo = (i: 0 | 1): void => setIndex(i);

  return (
    <div
      data-testid="mobile-deck-swiper"
      style={wrapStyle}
      onTouchStart={onTouchStart}
      onTouchMove={onTouchMove}
      onTouchEnd={onTouchEnd}
      onTouchCancel={(): void => {
        startXRef.current = null;
        deltaXRef.current = 0;
      }}
    >
      <div style={dotsRowStyle} role="tablist" aria-label="Deck selector">
        <button
          type="button"
          role="tab"
          aria-selected={index === 0}
          aria-label="Show Deck A"
          data-testid="mobile-deck-dot-A"
          onClick={(): void => goTo(0)}
          style={{
            ...dotStyle(index === 0),
            // 44 × 44 tap target around the 10 × 10 dot (a11y floor).
            padding: 17,
            cursor: "pointer",
            boxSizing: "content-box",
          }}
        />
        <button
          type="button"
          role="tab"
          aria-selected={index === 1}
          aria-label="Show Deck B"
          data-testid="mobile-deck-dot-B"
          onClick={(): void => goTo(1)}
          style={{
            ...dotStyle(index === 1),
            padding: 17,
            cursor: "pointer",
            boxSizing: "content-box",
          }}
        />
      </div>
      <div
        data-testid="mobile-deck-pane-A"
        style={deckWrapStyle(index === 0)}
        aria-hidden={index !== 0}
      >
        <Deck deck={decks[0]} side="left" client={client} />
      </div>
      <div
        data-testid="mobile-deck-pane-B"
        style={deckWrapStyle(index === 1)}
        aria-hidden={index !== 1}
      >
        <Deck deck={decks[1]} side="right" client={client} />
      </div>
    </div>
  );
};
