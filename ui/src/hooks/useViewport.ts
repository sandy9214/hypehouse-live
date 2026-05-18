// useViewport — viewport breakpoint detection for the responsive
// mobile/tablet/desktop layouts owned by `DeckRow`.
//
// We deliberately re-implement the well-known `useMediaQuery` shape
// rather than pulling a dep — three named breakpoints + a resize
// listener is ~20 lines of trivial code. Returns a stable enum so
// callers can `switch` (cleaner than juggling 3 booleans).
//
// Breakpoints (matches CSS media queries in DeckRow + MobileDeckSwiper):
//   - mobile  : < 768 px
//   - tablet  : 768-1023 px
//   - desktop : >= 1024 px
//
// SSR / jsdom: defaults to "desktop" when `window` is undefined so the
// existing 259 component tests (which never set innerWidth explicitly
// and render with the default jsdom width of 1024) keep their existing
// rendering. Tests for the responsive paths set `window.innerWidth`
// + `window.dispatchEvent(new Event("resize"))` to opt in.

import { useEffect, useState } from "react";

export type Breakpoint = "mobile" | "tablet" | "desktop";

export const MOBILE_MAX_PX = 767;
export const TABLET_MAX_PX = 1023;

export const breakpointFor = (widthPx: number): Breakpoint => {
  if (widthPx <= MOBILE_MAX_PX) return "mobile";
  if (widthPx <= TABLET_MAX_PX) return "tablet";
  return "desktop";
};

const readWidth = (): number => {
  if (typeof window === "undefined") return 1024;
  // `innerWidth` is 0 in headless environments that haven't laid out
  // yet — treat as desktop so existing test assertions stand.
  const w = window.innerWidth;
  return w > 0 ? w : 1024;
};

/** Subscribe to `window.resize` and re-derive the active breakpoint.
 * Returns a stable enum value — re-renders only on breakpoint changes,
 * not on every pixel of resize (we track width internally + bucket). */
export const useViewport = (): Breakpoint => {
  const [bp, setBp] = useState<Breakpoint>(
    (): Breakpoint => breakpointFor(readWidth()),
  );

  useEffect((): (() => void) => {
    if (typeof window === "undefined") return (): void => undefined;
    const handler = (): void => {
      const next = breakpointFor(readWidth());
      setBp((prev: Breakpoint): Breakpoint => (prev === next ? prev : next));
    };
    // Sync once on mount — covers the case where the initial state
    // read raced ahead of the first layout (rare but happens in test
    // harnesses that mutate innerWidth between render and effect).
    handler();
    window.addEventListener("resize", handler);
    return (): void => {
      window.removeEventListener("resize", handler);
    };
  }, []);

  return bp;
};
