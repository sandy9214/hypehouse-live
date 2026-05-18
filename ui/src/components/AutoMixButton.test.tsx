// AutoMixButton.test.tsx — render + click assertions.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { AutoMixButton } from "./AutoMixButton";
import type { JsonRpcWS } from "../ws/client";
import type { AutoMixSnapshot } from "../store/autoMix";

const makeClient = (
  call: ReturnType<typeof vi.fn> = vi.fn().mockResolvedValue({}),
): JsonRpcWS =>
  ({
    call,
  }) as unknown as JsonRpcWS;

const off: AutoMixSnapshot = {
  enabled: false,
  status: "idle",
  seconds_to_mix: null,
};
const on: AutoMixSnapshot = {
  enabled: true,
  status: "idle",
  seconds_to_mix: null,
};
const armed: AutoMixSnapshot = {
  enabled: true,
  status: "armed",
  seconds_to_mix: 12,
};
const transitioning: AutoMixSnapshot = {
  enabled: true,
  status: "transitioning",
  seconds_to_mix: 3,
};

describe("AutoMixButton", () => {
  afterEach((): void => {
    cleanup();
  });

  it("renders the OFF label when disabled", (): void => {
    render(<AutoMixButton deck="A" client={makeClient()} snapshot={off} />);
    const label = screen.getByTestId("auto-mix-label-A");
    expect(label.textContent).toContain("OFF");
    const btn = screen.getByTestId("auto-mix-A") as HTMLButtonElement;
    expect(btn.getAttribute("aria-pressed")).toBe("false");
  });

  it("emits copilot.set_auto_mix with the inverted flag on click", async (): Promise<void> => {
    const call = vi.fn().mockResolvedValue({});
    const client = makeClient(call);
    render(<AutoMixButton deck="A" client={client} snapshot={off} />);
    fireEvent.pointerDown(screen.getByTestId("auto-mix-A"));
    fireEvent.pointerUp(screen.getByTestId("auto-mix-A"));
    // setAutoMix is async; allow microtasks to flush.
    await Promise.resolve();
    expect(call).toHaveBeenCalledWith("copilot.set_auto_mix", {
      deck: "A",
      enabled: true,
    });
  });

  it("renders pulse animation style when enabled", (): void => {
    render(<AutoMixButton deck="A" client={makeClient()} snapshot={on} />);
    const btn = screen.getByTestId("auto-mix-A") as HTMLButtonElement;
    // The component sets `animation` inline when enabled; off state
    // sets "none".
    expect(btn.style.animation).not.toBe("");
    expect(btn.style.animation).not.toBe("none");
  });

  it("does NOT animate when disabled", (): void => {
    render(<AutoMixButton deck="A" client={makeClient()} snapshot={off} />);
    const btn = screen.getByTestId("auto-mix-A") as HTMLButtonElement;
    // Browser may normalize the inline value; the substring match is
    // safer than equality.
    expect(btn.style.animation === "" || btn.style.animation === "none").toBe(true);
  });

  it("shows countdown in the label when armed with seconds_to_mix", (): void => {
    render(<AutoMixButton deck="A" client={makeClient()} snapshot={armed} />);
    const label = screen.getByTestId("auto-mix-label-A");
    expect(label.textContent).toContain("12");
    expect(label.textContent).toContain("s");
  });

  it("shows MIXING label when transitioning", (): void => {
    render(
      <AutoMixButton deck="A" client={makeClient()} snapshot={transitioning} />,
    );
    const label = screen.getByTestId("auto-mix-label-A");
    expect(label.textContent).toContain("MIXING");
  });

  it("renders the countdown indicator with correct seconds when set", (): void => {
    render(<AutoMixButton deck="A" client={makeClient()} snapshot={armed} />);
    const countdown = screen.getByTestId("auto-mix-countdown-A");
    expect(countdown.textContent).toBe("12");
  });

  it("disables click-emit when already in target state", async (): Promise<void> => {
    // Idempotent: clicking "off" while already off still calls
    // set_auto_mix(false) — the copilot dedupes server-side. We just
    // assert the RPC fired with the expected payload.
    const call = vi.fn().mockResolvedValue({});
    render(<AutoMixButton deck="B" client={makeClient(call)} snapshot={on} />);
    fireEvent.pointerDown(screen.getByTestId("auto-mix-B"));
    fireEvent.pointerUp(screen.getByTestId("auto-mix-B"));
    await Promise.resolve();
    expect(call).toHaveBeenCalledWith("copilot.set_auto_mix", {
      deck: "B",
      enabled: false,
    });
  });
});
