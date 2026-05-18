// Shared Button component with optional long-press detection.
//
// Used by deck transport (play/pause/cue), hot-cue pads (long-press =
// HotCueSet, short-press = HotCueTrigger), loop in/out, and the
// copilot toggle. The long-press threshold defaults to 500ms per the
// PR brief; callers can tune via prop if they need a different feel.
//
// Behavior:
//   - `onClick` fires on a short press (pointerup before threshold).
//   - `onLongPress` fires once when the threshold elapses while still
//     held. After it fires, the matching `onClick` is suppressed so the
//     caller doesn't see both for one gesture.
//   - Keyboard activation (Enter / Space) maps to `onClick`. Long-press
//     is intentionally pointer-only — it's a continuous gesture, not a
//     discrete key event, and tests assert pointerdown/up directly.

import type {
  CSSProperties,
  KeyboardEvent,
  PointerEvent,
  ReactNode,
} from "react";
import { useCallback, useRef } from "react";

export interface ButtonProps {
  onClick?: () => void;
  onLongPress?: () => void;
  longPressMs?: number;
  pressed?: boolean;
  disabled?: boolean;
  ariaLabel?: string;
  title?: string;
  testId?: string;
  style?: CSSProperties;
  children: ReactNode;
}

const baseStyle: CSSProperties = {
  border: "1px solid #333",
  background: "#1f1f1f",
  color: "#ddd",
  padding: "6px 10px",
  fontFamily: "monospace",
  fontSize: 12,
  cursor: "pointer",
  userSelect: "none",
  borderRadius: 3,
};

const pressedStyle: CSSProperties = {
  background: "#2a5a8a",
  borderColor: "#3a7ab0",
  color: "#fff",
};

const disabledStyle: CSSProperties = {
  opacity: 0.4,
  cursor: "not-allowed",
};

export const Button = ({
  onClick,
  onLongPress,
  longPressMs = 500,
  pressed = false,
  disabled = false,
  ariaLabel,
  title,
  testId,
  style,
  children,
}: ButtonProps): JSX.Element => {
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const firedLongRef = useRef<boolean>(false);

  const clearTimer = useCallback((): void => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  }, []);

  const handlePointerDown = useCallback(
    (_ev: PointerEvent<HTMLButtonElement>): void => {
      if (disabled) return;
      firedLongRef.current = false;
      if (!onLongPress) return;
      clearTimer();
      timerRef.current = setTimeout((): void => {
        firedLongRef.current = true;
        onLongPress();
      }, longPressMs);
    },
    [disabled, onLongPress, longPressMs, clearTimer],
  );

  const handlePointerUp = useCallback(
    (_ev: PointerEvent<HTMLButtonElement>): void => {
      if (disabled) return;
      clearTimer();
      if (firedLongRef.current) {
        // Long-press already handled — suppress the click.
        firedLongRef.current = false;
        return;
      }
      onClick?.();
    },
    [disabled, onClick, clearTimer],
  );

  const handlePointerLeave = useCallback((): void => {
    // Abort an in-flight long-press; do NOT fire click on leave.
    clearTimer();
    firedLongRef.current = false;
  }, [clearTimer]);

  const handleKeyDown = useCallback(
    (ev: KeyboardEvent<HTMLButtonElement>): void => {
      if (disabled) return;
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        onClick?.();
      }
    },
    [disabled, onClick],
  );

  const computed: CSSProperties = {
    ...baseStyle,
    ...(pressed ? pressedStyle : {}),
    ...(disabled ? disabledStyle : {}),
    ...style,
  };

  return (
    <button
      type="button"
      aria-label={ariaLabel}
      aria-pressed={pressed}
      title={title}
      disabled={disabled}
      data-testid={testId}
      style={computed}
      onPointerDown={handlePointerDown}
      onPointerUp={handlePointerUp}
      onPointerLeave={handlePointerLeave}
      onKeyDown={handleKeyDown}
    >
      {children}
    </button>
  );
};
