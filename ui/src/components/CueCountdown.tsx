// CueCountdown — pro-DJ "next downbeat" indicator.
//
// In a live mix, hitting Play on the off-bar is the newbie tell —
// pros slam transport on the next downbeat. Pioneer / Denon / Traktor
// all surface a sub-frame "ms-to-next-bar" readout. We use:
//   - `extrapolatedPosition(deckId)` — sub-frame playhead (engine ~5Hz,
//     rAF ~60Hz; store extrapolates between pushes).
//   - `downbeatsMs[]` — analyzer output, sorted ascending, captured at
//     DeckLoad time in Deck.tsx onDrop.
//
// Display states:
//   - >2 s         → "Next bar in 3.2s"   (calm, peripheral)
//   - ≤2 s         → "3" / "2" / "1" / "0" big flashing digit (focus)
//   - hit downbeat → "BAR" flash ~250 ms  (confirmation)
//
// Phrase indicator (default 16-bar EDM phrase) shows current bar mod
// phraseBars so the DJ can time transitions to phrase boundaries.
//
// rAF loop is identity-stable across renders; cleanup on unmount.
// DOM mutation (not setState) inside the loop — same idiom as
// Waveform.tsx's bar/beat label, avoids 60 Hz React reconciles.

import { useEffect, useMemo, useRef } from "react";
import type { CSSProperties, JSX } from "react";
import type { DeckId } from "../store/engine";
import { extrapolatedPosition } from "../store/engine";

export interface CueCountdownProps {
  deck: DeckId;
  downbeatsMs: ReadonlyArray<number>;
  beatPeriodMs: number;
  /** Override the extrapolator (tests). */
  positionProvider?: () => number;
  /** Phrase length in bars. Default 16 (typical EDM phrase). */
  phraseBars?: number;
  /** Component width (px). */
  width?: number;
}

/** Threshold (ms) below which we flip to big-digit countdown mode. */
export const COUNTDOWN_THRESHOLD_MS = 2_000;
/** Window (ms) after a downbeat crossing where we flash "BAR". */
export const BAR_FLASH_WINDOW_MS = 250;

/** ms-to-next-downbeat, or `null` if no downbeat ahead. Linear scan
 * is fine — analyzer ships ~hundreds per track, runs once per rAF. */
export const msToNextDownbeat = (
  positionMs: number,
  downbeatsMs: ReadonlyArray<number>,
): number | null => {
  for (const d of downbeatsMs) {
    if (d >= positionMs) return d - positionMs;
  }
  return null;
};

/** Count of downbeats at-or-before `positionMs` (1-based bar index). */
export const currentBarIndex = (
  positionMs: number,
  downbeatsMs: ReadonlyArray<number>,
): number => {
  let n = 0;
  for (const d of downbeatsMs) {
    if (d <= positionMs) n++;
    else break;
  }
  return n;
};

/** One-decimal so the readout visibly counts down at low frame rates. */
export const formatSecondsReadout = (ms: number): string =>
  `Next bar in ${(ms / 1000).toFixed(1)}s`;

/** ms-remaining → big-digit display. 2000-1500 → "3", 1500-1000 → "2",
 * 1000-500 → "1", 500-0 → "0". */
export const countdownDigit = (ms: number): "3" | "2" | "1" | "0" => {
  if (ms > 1500) return "3";
  if (ms > 1000) return "2";
  if (ms > 500) return "1";
  return "0";
};

const wrapStyle = (width: number): CSSProperties => ({
  width, display: "flex", alignItems: "center", justifyContent: "space-between",
  padding: "4px 8px", background: "#0a0e14", border: "1px solid #1f2a36",
  borderRadius: 3, fontFamily: "monospace", color: "#9ab", fontSize: 12,
  userSelect: "none", minHeight: 24,
});

// "0" + "BAR" go hot-yellow so they pop in peripheral vision; the
// 3/2/1 ramp stays cooler so the eye registers "approaching" vs "now".
const bigDigitStyle = (digit: string): string =>
  `font-size: 22px; font-weight: 700; letter-spacing: 2px; ` +
  `font-variant-numeric: tabular-nums; ` +
  `color: ${digit === "0" ? "#ffe14a" : "#6ab0ff"}; ` +
  `animation: cueCountdownFlash 0.5s ease-in-out infinite`;

const BAR_FLASH_CSS =
  "font-size: 22px; font-weight: 800; color: #ffe14a; letter-spacing: 3px; " +
  "animation: cueCountdownBarFlash 0.25s ease-out";

const SECONDS_CSS = "font-size: 12px; color: #9ab";
const IDLE_CSS = "font-size: 12px; color: #445";

const phraseStyle: CSSProperties = {
  fontSize: 11, color: "#6b7a8a", fontVariantNumeric: "tabular-nums",
};

/** Inject the @keyframes once per page (idempotent — same trick as
 * BpmLockBadge). */
const ensureKeyframes = (): void => {
  if (typeof document === "undefined") return;
  const id = "cue-countdown-keyframes";
  if (document.getElementById(id) !== null) return;
  const style = document.createElement("style");
  style.id = id;
  style.textContent =
    "@keyframes cueCountdownFlash { 0%,100% { opacity: 1 } 50% { opacity: 0.4 } } " +
    "@keyframes cueCountdownBarFlash { 0% { opacity: 1; transform: scale(1.2) } 100% { opacity: 1; transform: scale(1) } }";
  document.head.appendChild(style);
};

export const CueCountdown = ({
  deck, downbeatsMs, beatPeriodMs, positionProvider,
  phraseBars = 16, width = 480,
}: CueCountdownProps): JSX.Element => {
  ensureKeyframes();
  const readoutRef = useRef<HTMLSpanElement | null>(null);
  const phraseRef = useRef<HTMLSpanElement | null>(null);
  const wrapRef = useRef<HTMLDivElement | null>(null);
  // Latch latest props into a ref so the rAF closure isn't re-armed
  // each render (mirrors Waveform.tsx).
  const propsRef = useRef({ deck, downbeatsMs, beatPeriodMs, positionProvider, phraseBars });
  propsRef.current = { deck, downbeatsMs, beatPeriodMs, positionProvider, phraseBars };
  // beatPeriodMs not consumed in the rAF body today but kept in the
  // ref + prop shape for future "snap to grid" features (e.g. half-bar
  // markers). Suppress unused-local TS warning with explicit void.
  void beatPeriodMs;

  // Stable downbeats so identity only flips on actual swap.
  const stableDownbeats = useMemo(
    (): ReadonlyArray<number> => downbeatsMs, [downbeatsMs],
  );

  useEffect((): (() => void) => {
    let raf = 0;
    // Track last bar-index — when it ticks up we just crossed a
    // downbeat, fire BAR flash. Ref-local closure beats re-rendering
    // on each bar (DOM mutation, no React reconcile).
    let lastBarIndex = -1;
    let barFlashUntil = 0;
    const tick = (): void => {
      const p = propsRef.current;
      const provider = p.positionProvider
        ? p.positionProvider
        : (): number => extrapolatedPosition(p.deck);
      const pos = provider();
      const dt = msToNextDownbeat(pos, p.downbeatsMs);
      const barIdx = currentBarIndex(pos, p.downbeatsMs);
      const now = typeof performance !== "undefined"
        ? performance.now() : Date.now();

      // Detect downbeat crossing → kick BAR flash window. Initial
      // lastBarIndex = -1 so first tick doesn't fire on mount.
      if (barIdx > lastBarIndex && lastBarIndex !== -1) {
        barFlashUntil = now + BAR_FLASH_WINDOW_MS;
      }
      lastBarIndex = barIdx;

      const readout = readoutRef.current;
      const phrase = phraseRef.current;
      const wrap = wrapRef.current;
      if (!readout || !phrase || !wrap) {
        raf = requestAnimationFrame(tick);
        return;
      }

      // BAR flash takes precedence over other states.
      if (now < barFlashUntil) {
        readout.textContent = "BAR";
        readout.setAttribute("style", BAR_FLASH_CSS);
        wrap.setAttribute("data-state", "bar-flash");
      } else if (dt === null || p.downbeatsMs.length === 0) {
        // No future downbeat (end of track / no grid) → blank.
        readout.textContent = "—";
        readout.setAttribute("style", IDLE_CSS);
        wrap.setAttribute("data-state", "idle");
      } else if (dt < COUNTDOWN_THRESHOLD_MS) {
        // <2 s → big flashing digit countdown.
        const digit = countdownDigit(dt);
        readout.textContent = digit;
        readout.setAttribute("style", bigDigitStyle(digit));
        wrap.setAttribute("data-state", "countdown");
        wrap.setAttribute("data-digit", digit);
      } else {
        // >2 s → calm seconds readout.
        readout.textContent = formatSecondsReadout(dt);
        readout.setAttribute("style", SECONDS_CSS);
        wrap.setAttribute("data-state", "seconds");
      }

      // Phrase indicator: 1-indexed bar-in-phrase ("Bar N of M").
      if (p.downbeatsMs.length > 0 && barIdx > 0) {
        const inPhrase = ((barIdx - 1) % p.phraseBars) + 1;
        phrase.textContent = `Bar ${inPhrase} of ${p.phraseBars}`;
      } else {
        phrase.textContent = `Bar — of ${p.phraseBars}`;
      }

      raf = requestAnimationFrame(tick);
    };
    tick();
    return (): void => cancelAnimationFrame(raf);
  }, [stableDownbeats]);

  return (
    <div
      ref={wrapRef}
      data-testid={`cue-countdown-${deck}`}
      data-state="idle"
      style={wrapStyle(width)}
      aria-label={`Cue countdown for deck ${deck}`}
      role="status"
    >
      <span ref={readoutRef} data-testid={`cue-countdown-${deck}-readout`}>
        —
      </span>
      <span
        ref={phraseRef}
        data-testid={`cue-countdown-${deck}-phrase`}
        style={phraseStyle}
      >
        Bar — of {phraseBars}
      </span>
    </div>
  );
};
