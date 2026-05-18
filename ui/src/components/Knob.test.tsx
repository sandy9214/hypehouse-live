// Knob.test.tsx — slider input fires onChange with parsed numeric
// value; double-click resets to `resetValue`.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Knob } from "./Knob";

describe("Knob", () => {
  afterEach((): void => {
    cleanup();
  });

  it("calls onChange with numeric value on drag/input", (): void => {
    const onChange = vi.fn();
    render(
      <Knob
        label="PITCH"
        min={-12}
        max={12}
        step={0.1}
        value={0}
        onChange={onChange}
        testId="k"
      />,
    );
    const input = screen.getByTestId("k-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "3.5" } });
    expect(onChange).toHaveBeenCalledWith(3.5);
  });

  it("resets on double-click to resetValue", (): void => {
    const onChange = vi.fn();
    render(
      <Knob
        label="EQ"
        min={-26}
        max={12}
        step={0.5}
        value={5}
        onChange={onChange}
        resetValue={0}
        testId="k"
      />,
    );
    fireEvent.doubleClick(screen.getByTestId("k-input"));
    expect(onChange).toHaveBeenCalledWith(0);
  });

  it("resets on middle-click", (): void => {
    const onChange = vi.fn();
    render(
      <Knob
        label="EQ"
        min={-26}
        max={12}
        step={0.5}
        value={7}
        onChange={onChange}
        resetValue={0}
        testId="k"
      />,
    );
    fireEvent.mouseDown(screen.getByTestId("k-input"), { button: 1 });
    expect(onChange).toHaveBeenCalledWith(0);
  });

  it("renders the formatted value", (): void => {
    render(
      <Knob
        label="PITCH"
        min={-12}
        max={12}
        step={0.1}
        value={-2.5}
        onChange={(): void => undefined}
        format={(v): string => `${v.toFixed(1)} st`}
        testId="k"
      />,
    );
    expect(screen.getByTestId("k-value").textContent).toBe("-2.5 st");
  });
});
