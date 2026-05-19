// SessionsExportButton.test.tsx — disabled-when-no-recording, click
// dispatches engine.export_session, success/failure banners.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { SessionsExportButton } from "./SessionsExportButton";
import type { JsonRpcWS } from "../ws/client";

type Call = (method: string, params?: unknown) => Promise<unknown>;

const makeClient = (
  responses: Record<string, unknown>,
): { client: JsonRpcWS; call: ReturnType<typeof vi.fn> } => {
  const call = vi.fn<Call>((method: string): Promise<unknown> => {
    if (method in responses) {
      const v = responses[method];
      if (v instanceof Error) return Promise.reject(v);
      return Promise.resolve(v);
    }
    return Promise.reject(new Error(`unmocked: ${method}`));
  });
  return { client: { call } as unknown as JsonRpcWS, call };
};

const successSummary = {
  input_duration_s: 30.0,
  output_duration_s: 22.0,
  trimmed_head_s: 5.0,
  trimmed_tail_s: 3.0,
  chapter_count: 4,
  output_path: "/Users/me/Downloads/session-1.wav",
  chapters_path: "/Users/me/Downloads/session-1.wav.chapters.txt",
};

describe("SessionsExportButton", () => {
  beforeEach((): void => {
    vi.clearAllMocks();
  });
  afterEach((): void => {
    cleanup();
  });

  it("button is disabled when the session has no recording", (): void => {
    const { client } = makeClient({});
    render(
      <SessionsExportButton
        client={client}
        sessionId="20260518T010101Z-aaaa"
        hasRecording={false}
      />,
    );
    const btn = screen.getByTestId(
      "sessions-export-btn-20260518T010101Z-aaaa",
    ) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    // Title hint surfaces why.
    expect(btn.title.toLowerCase()).toContain("no master.wav");
  });

  it("click dispatches engine.export_session and shows the saved-to toast on success", async (): Promise<void> => {
    const { client, call } = makeClient({
      "engine.export_session": successSummary,
    });
    const sid = "20260518T010101Z-bbbb";
    render(
      <SessionsExportButton client={client} sessionId={sid} hasRecording={true} />,
    );
    fireEvent.click(screen.getByTestId(`sessions-export-btn-${sid}`));
    // RPC fired with the right params.
    await waitFor((): void => {
      const c = call.mock.calls.find(
        (c: unknown[]): boolean => c[0] === "engine.export_session",
      );
      expect(c).toBeDefined();
      expect(c?.[1]).toEqual({ session_id: sid });
    });
    // Success toast appears with the saved-to path + chapter count.
    await waitFor((): void => {
      expect(screen.getByTestId(`sessions-export-done-${sid}`)).toBeTruthy();
    });
    const toast = screen.getByTestId(`sessions-export-done-${sid}`);
    expect(toast.textContent).toContain("/Users/me/Downloads/session-1.wav");
    expect(toast.textContent).toContain("4 chapters");
  });

  it("RPC failure shows an error toast and the button re-enables", async (): Promise<void> => {
    const { client } = makeClient({
      "engine.export_session": new Error("master.wav missing"),
    });
    const sid = "20260518T010101Z-cccc";
    render(
      <SessionsExportButton client={client} sessionId={sid} hasRecording={true} />,
    );
    fireEvent.click(screen.getByTestId(`sessions-export-btn-${sid}`));
    await waitFor((): void => {
      expect(screen.getByTestId(`sessions-export-error-${sid}`)).toBeTruthy();
    });
    expect(
      screen.getByTestId(`sessions-export-error-${sid}`).textContent,
    ).toContain("master.wav missing");
    // Button is back to enabled (not disabled).
    const btn = screen.getByTestId(
      `sessions-export-btn-${sid}`,
    ) as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });
});
