// Button.test.tsx — short-press fires onClick, long-press fires
// onLongPress after 500ms (and suppresses the click).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Button } from "./Button";

describe("Button", () => {
  beforeEach((): void => {
    vi.useFakeTimers();
  });
  afterEach((): void => {
    vi.useRealTimers();
    cleanup();
  });

  it("fires onClick on a short press", (): void => {
    const onClick = vi.fn();
    render(
      <Button onClick={onClick} testId="b">
        hi
      </Button>,
    );
    const btn = screen.getByTestId("b");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("fires onLongPress after 500ms and suppresses click", (): void => {
    const onClick = vi.fn();
    const onLongPress = vi.fn();
    render(
      <Button onClick={onClick} onLongPress={onLongPress} testId="b">
        hi
      </Button>,
    );
    const btn = screen.getByTestId("b");
    fireEvent.pointerDown(btn);
    vi.advanceTimersByTime(500);
    fireEvent.pointerUp(btn);
    expect(onLongPress).toHaveBeenCalledTimes(1);
    expect(onClick).not.toHaveBeenCalled();
  });

  it("does not fire onLongPress if released before threshold", (): void => {
    const onClick = vi.fn();
    const onLongPress = vi.fn();
    render(
      <Button onClick={onClick} onLongPress={onLongPress} testId="b">
        hi
      </Button>,
    );
    const btn = screen.getByTestId("b");
    fireEvent.pointerDown(btn);
    vi.advanceTimersByTime(200);
    fireEvent.pointerUp(btn);
    expect(onLongPress).not.toHaveBeenCalled();
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("ignores interaction when disabled", (): void => {
    const onClick = vi.fn();
    const onLongPress = vi.fn();
    render(
      <Button
        onClick={onClick}
        onLongPress={onLongPress}
        disabled
        testId="b"
      >
        hi
      </Button>,
    );
    const btn = screen.getByTestId("b");
    fireEvent.pointerDown(btn);
    vi.advanceTimersByTime(600);
    fireEvent.pointerUp(btn);
    expect(onClick).not.toHaveBeenCalled();
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("reflects pressed state via aria-pressed", (): void => {
    render(
      <Button pressed testId="b">
        hi
      </Button>,
    );
    expect(screen.getByTestId("b").getAttribute("aria-pressed")).toBe("true");
  });
});
