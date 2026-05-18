// Shared vertical-slider "knob" for continuous controls (pitch + EQ).
//
// v0.1 implements as a vertical native range input so we get keyboard
// support + ARIA semantics + browser touch handling for free. The
// `onChange` fires on every drag tick; the parent decides whether to
// debounce or fire-and-forget RPCs.
//
// Reset behaviour:
//   - double-click resets to `resetValue ?? 0`.
//   - middle-click (button === 1) also resets — keeps parity with the
//     "center-click-to-reset" expectation from the PR brief for the
//     pitch slider.
//
// Keyboard:
//   - native range input handles ArrowUp/Down/PgUp/PgDn by `step`.

import type { ChangeEvent, CSSProperties, MouseEvent } from "react";
import { useCallback } from "react";

export interface KnobProps {
  label: string;
  min: number;
  max: number;
  step: number;
  value: number;
  onChange: (next: number) => void;
  /** Value applied on double-click / middle-click. Defaults to 0. */
  resetValue?: number;
  /** Render with vertical orientation (default true). */
  vertical?: boolean;
  /** Optional formatter for the value-readout. */
  format?: (v: number) => string;
  /** Width in px for horizontal layout / height for vertical. */
  size?: number;
  testId?: string;
  ariaLabel?: string;
}

const wrapStyle: CSSProperties = {
  display: "inline-flex",
  flexDirection: "column",
  alignItems: "center",
  gap: 2,
  fontFamily: "monospace",
  fontSize: 11,
  color: "#bbb",
};

const verticalInputStyle = (size: number): CSSProperties => ({
  writingMode: "vertical-lr",
  // The vendor flag is the only way to get a true vertical native
  // slider in Chromium / WebKit today. Safe to set unconditionally.
  WebkitAppearance: "slider-vertical",
  height: size,
  width: 24,
  cursor: "ns-resize",
});

const defaultFormat = (v: number): string => v.toFixed(2);

export const Knob = ({
  label,
  min,
  max,
  step,
  value,
  onChange,
  resetValue = 0,
  vertical = true,
  format = defaultFormat,
  size = 90,
  testId,
  ariaLabel,
}: KnobProps): JSX.Element => {
  const handleChange = useCallback(
    (ev: ChangeEvent<HTMLInputElement>): void => {
      const next = Number(ev.target.value);
      if (!Number.isFinite(next)) return;
      onChange(next);
    },
    [onChange],
  );

  const handleDoubleClick = useCallback((): void => {
    onChange(resetValue);
  }, [onChange, resetValue]);

  const handleMouseDown = useCallback(
    (ev: MouseEvent<HTMLInputElement>): void => {
      // Middle-click = reset. Equivalent to the "center-click" affordance.
      if (ev.button === 1) {
        ev.preventDefault();
        onChange(resetValue);
      }
    },
    [onChange, resetValue],
  );

  const style: CSSProperties = vertical
    ? verticalInputStyle(size)
    : { width: size };

  return (
    <div style={wrapStyle} data-testid={testId}>
      <span aria-hidden="true">{label}</span>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={handleChange}
        onDoubleClick={handleDoubleClick}
        onMouseDown={handleMouseDown}
        aria-label={ariaLabel ?? label}
        aria-valuemin={min}
        aria-valuemax={max}
        aria-valuenow={value}
        data-testid={testId ? `${testId}-input` : undefined}
        style={style}
      />
      <span data-testid={testId ? `${testId}-value` : undefined}>
        {format(value)}
      </span>
    </div>
  );
};
