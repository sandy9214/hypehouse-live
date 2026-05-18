// Toaster.test.tsx — verify decode-error toasts render, auto-dismiss,
// stack to a cap, and respond to manual dismissal.
//
// The store + the component are tested end-to-end here: we fire a real
// JSON-RPC notification through `applyDecodeErrorNotification`, then
// assert on the rendered DOM. Vitest's fake-timer pump exercises the
// 5s auto-dismiss without sleeping for real wall time.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Toaster } from "./Toaster";
import {
  __resetDecodeErrors,
  applyDecodeErrorNotification,
  DECODE_ERROR_AUTO_DISMISS_MS,
} from "../store/notifications";

const fireDecodeError = (overrides: {
  deck?: "A" | "B";
  track_id?: string;
  category?: string;
  error?: string;
} = {}): void => {
  applyDecodeErrorNotification({
    jsonrpc: "2.0",
    method: "engine.decode_error",
    params: {
      deck: overrides.deck ?? "A",
      track_id: overrides.track_id ?? "track-x",
      category: overrides.category ?? "file_not_found",
      error: overrides.error ?? "io error opening /missing.mp3",
    },
  });
};

describe("Toaster", () => {
  beforeEach((): void => {
    vi.useFakeTimers();
  });

  afterEach((): void => {
    act((): void => {
      __resetDecodeErrors();
    });
    cleanup();
    vi.useRealTimers();
  });

  it("renders nothing when the queue is empty", (): void => {
    const { container } = render(<Toaster />);
    expect(container.firstChild).toBeNull();
    expect(screen.queryByTestId("toaster-root")).toBeNull();
  });

  it("renders a toast when an engine.decode_error notification arrives", (): void => {
    render(<Toaster />);
    act((): void => {
      fireDecodeError({
        deck: "A",
        track_id: "abc-123",
        category: "file_not_found",
        error: "io error opening /missing.mp3",
      });
    });
    const root = screen.getByTestId("toaster-root");
    expect(root).toBeTruthy();
    // The label maps file_not_found → "File not found"; deck shows in
    // the header, error in the body.
    expect(screen.getByRole("alert").textContent).toContain("File not found");
    expect(screen.getByRole("alert").textContent).toContain("Deck A");
    expect(screen.getByRole("alert").textContent).toContain(
      "io error opening /missing.mp3",
    );
    expect(screen.getByRole("alert").textContent).toContain("abc-123");
  });

  it("auto-dismisses a toast after the auto-dismiss window", (): void => {
    render(<Toaster />);
    act((): void => {
      fireDecodeError();
    });
    expect(screen.queryByRole("alert")).toBeTruthy();
    act((): void => {
      vi.advanceTimersByTime(DECODE_ERROR_AUTO_DISMISS_MS + 100);
    });
    expect(screen.queryByRole("alert")).toBeNull();
    // The toaster root unmounts when the queue empties.
    expect(screen.queryByTestId("toaster-root")).toBeNull();
  });

  it("dismisses immediately when the user clicks the X button", (): void => {
    render(<Toaster />);
    act((): void => {
      fireDecodeError();
    });
    const dismiss = screen.getByLabelText("Dismiss notification");
    fireEvent.click(dismiss);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("stacks at most three toasts and shows the newest ones", (): void => {
    render(<Toaster />);
    act((): void => {
      // Five errors land in quick succession.
      fireDecodeError({ track_id: "t1", error: "err1" });
      fireDecodeError({ track_id: "t2", error: "err2" });
      fireDecodeError({ track_id: "t3", error: "err3" });
      fireDecodeError({ track_id: "t4", error: "err4" });
      fireDecodeError({ track_id: "t5", error: "err5" });
    });
    const toasts = screen.getAllByRole("alert");
    expect(toasts).toHaveLength(3);
    // Newest-first ordering: top toast = t5, then t4, then t3.
    expect(toasts[0].textContent).toContain("err5");
    expect(toasts[1].textContent).toContain("err4");
    expect(toasts[2].textContent).toContain("err3");
  });

  it("rejects malformed notifications without crashing", (): void => {
    render(<Toaster />);
    act((): void => {
      applyDecodeErrorNotification({
        jsonrpc: "2.0",
        method: "engine.decode_error",
        params: { deck: "C", track_id: 7 as unknown as string },
      });
      applyDecodeErrorNotification({
        jsonrpc: "2.0",
        method: "engine.decode_error",
        params: { deck: "A" },
      });
      applyDecodeErrorNotification({
        jsonrpc: "2.0",
        method: "engine.unrelated",
        params: { deck: "A", track_id: "t", error: "should-not-toast" },
      });
    });
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("falls back to a generic decoder-error label when category is unknown", (): void => {
    render(<Toaster />);
    act((): void => {
      fireDecodeError({
        category: "totally_new_category_we_have_not_seen",
      });
    });
    // The Toaster's categoryLabel default branch maps unknowns to
    // "Decode error" so the toast still reads naturally even when the
    // engine adds a new variant before the UI ships a label for it.
    expect(screen.getByRole("alert").textContent).toContain("Decode error");
  });
});
