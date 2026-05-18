// KeyMatchButton.test.tsx — click-flow + state-machine assertions.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { KeyMatchButton } from "./KeyMatchButton";
import type { JsonRpcWS } from "../ws/client";

interface MockCall {
  (method: string, params?: unknown): Promise<unknown>;
}

const makeClient = (
  call: MockCall = vi.fn().mockResolvedValue({ semitones: 0 }),
): JsonRpcWS =>
  ({
    call,
  }) as unknown as JsonRpcWS;

const flush = async (): Promise<void> => {
  // Await two microtask hops — once for the compute_offset await,
  // once for the submit_event await — before assertions on call() args.
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
};

describe("KeyMatchButton", () => {
  afterEach((): void => {
    cleanup();
  });

  it("renders disabled with reduced opacity when either deck has no track", () => {
    render(
      <KeyMatchButton
        deck="B"
        thisKey={null}
        thisTrackId={null}
        otherKey="8B"
        otherTrackId="t1"
        client={makeClient()}
      />,
    );
    const btn = screen.getByTestId("key-match-B") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    // Reduced opacity = disabled-style branch.
    expect(btn.style.opacity).not.toBe("");
  });

  it("renders disabled when either key is unparseable", () => {
    render(
      <KeyMatchButton
        deck="B"
        thisKey="?"
        thisTrackId="t2"
        otherKey="8B"
        otherTrackId="t1"
        client={makeClient()}
      />,
    );
    const btn = screen.getByTestId("key-match-B") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it("renders enabled 'Key →' when both decks have valid keys", () => {
    render(
      <KeyMatchButton
        deck="B"
        thisKey="9B"
        thisTrackId="t2"
        otherKey="8B"
        otherTrackId="t1"
        client={makeClient()}
      />,
    );
    const btn = screen.getByTestId("key-match-B") as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
    expect(btn.textContent).toContain("Key");
  });

  it("calls key_match.compute_offset then submit_event PitchBend on click", async () => {
    const call = vi
      .fn()
      .mockResolvedValueOnce({ semitones: -5 })
      .mockResolvedValueOnce({});
    const client = makeClient(call);
    render(
      <KeyMatchButton
        deck="B"
        thisKey="9B"
        thisTrackId="t2"
        otherKey="8B"
        otherTrackId="t1"
        client={client}
      />,
    );
    fireEvent.click(screen.getByTestId("key-match-B"));
    await flush();
    expect(call).toHaveBeenNthCalledWith(1, "key_match.compute_offset", {
      from_track_id: "t2",
      to_track_id: "t1",
    });
    expect(call).toHaveBeenNthCalledWith(2, "submit_event", {
      PitchBend: { deck: "B", semitones: -5 },
    });
  });

  it("shows 'Matched' badge after a successful flow", async () => {
    const call = vi
      .fn()
      .mockResolvedValueOnce({ semitones: 2 })
      .mockResolvedValueOnce({});
    const client = makeClient(call);
    render(
      <KeyMatchButton
        deck="A"
        thisKey="8B"
        thisTrackId="ta"
        otherKey="10B"
        otherTrackId="tb"
        client={client}
      />,
    );
    fireEvent.click(screen.getByTestId("key-match-A"));
    await flush();
    const btn = screen.getByTestId("key-match-A");
    expect(btn.getAttribute("data-state")).toBe("matched");
    expect(btn.textContent).toContain("Matched");
  });

  it("re-arms (state idle) when the RPC rejects", async () => {
    const call = vi.fn().mockRejectedValue(new Error("network down"));
    const client = makeClient(call);
    render(
      <KeyMatchButton
        deck="B"
        thisKey="9B"
        thisTrackId="t2"
        otherKey="8B"
        otherTrackId="t1"
        client={client}
      />,
    );
    fireEvent.click(screen.getByTestId("key-match-B"));
    await flush();
    const btn = screen.getByTestId("key-match-B") as HTMLButtonElement;
    expect(btn.getAttribute("data-state")).toBe("idle");
    expect(btn.disabled).toBe(false);
  });

  it("does not call the RPC when disabled", () => {
    const call = vi.fn();
    const client = makeClient(call);
    render(
      <KeyMatchButton
        deck="B"
        thisKey={null}
        thisTrackId={null}
        otherKey="8B"
        otherTrackId="t1"
        client={client}
      />,
    );
    fireEvent.click(screen.getByTestId("key-match-B"));
    expect(call).not.toHaveBeenCalled();
  });
});
