// Sessions.test.tsx — History panel renders rows from mock data, click
// row opens replay, loading + empty states.

import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { Sessions } from "./Sessions";
import type { JsonRpcWS } from "../ws/client";
import {
  __resetSessionsStore,
  type SessionSummary,
} from "../store/sessions";

const makeSummary = (
  id: string,
  extra: Partial<SessionSummary> = {},
): SessionSummary => ({
  id,
  started_at_micros: 1_700_000_000_000_000,
  ended_at_micros: 1_700_000_300_000_000, // 5 min later
  event_count: 42,
  has_recording: true,
  recording_size_bytes: 4_194_304, // 4 MB
  ...extra,
});

type Call = (method: string, params?: unknown) => Promise<unknown>;

const makeClient = (
  responses: Record<string, unknown>,
): {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
} => {
  const call = vi.fn<Call>((method: string): Promise<unknown> => {
    if (method in responses) return Promise.resolve(responses[method]);
    return Promise.reject(new Error(`unmocked: ${method}`));
  });
  return { client: { call } as unknown as JsonRpcWS, call };
};

describe("Sessions", () => {
  beforeEach((): void => {
    __resetSessionsStore();
  });
  afterEach((): void => {
    cleanup();
    __resetSessionsStore();
  });

  it("renders one row per fetched session with metadata", async (): Promise<void> => {
    const sessions = [
      makeSummary("20260518T013312Z-aaaa", { event_count: 100 }),
      makeSummary("20260518T015555Z-bbbb", {
        has_recording: false,
        recording_size_bytes: null,
      }),
    ];
    const { client } = makeClient({
      "engine.list_sessions": { sessions },
    });
    render(<Sessions client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId(`sessions-row-${sessions[0].id}`)).toBeTruthy();
    });
    expect(screen.getByTestId(`sessions-row-${sessions[1].id}`)).toBeTruthy();
    // Recording badge present on the first session, "none" badge on the second.
    const rowOne = screen.getByTestId(`sessions-row-${sessions[0].id}`);
    expect(rowOne.textContent).toContain("100"); // event count
    const rowTwo = screen.getByTestId(`sessions-row-${sessions[1].id}`);
    expect(rowTwo.textContent?.toLowerCase()).toContain("none");
  });

  it("clicking a row triggers engine.replay_session and shows the snapshot", async (): Promise<void> => {
    const sessions = [makeSummary("20260518T013312Z-aaaa", { event_count: 7 })];
    const replayState = {
      session_active: true,
      master_bpm: 125.5,
      crossfader: 0.42,
      deck_a: {
        loaded: { id: "track-1", path: "/m/track-1.mp3" },
        playing: true,
        bpm: 128.0,
        position_ms: 0,
        hot_cues: [0, null, null, null, null, null, null, null],
        copilot_engaged: false,
      },
      deck_b: {
        loaded: null,
        playing: false,
        bpm: 0,
        position_ms: 0,
        hot_cues: [null, null, null, null, null, null, null, null],
        copilot_engaged: false,
      },
    };
    const { client, call } = makeClient({
      "engine.list_sessions": { sessions },
      "engine.replay_session": { state: replayState, event_count: 7 },
    });
    render(<Sessions client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId(`sessions-row-${sessions[0].id}`)).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId(`sessions-row-${sessions[0].id}`));
    await waitFor((): void => {
      expect(screen.getByTestId("sessions-replay-snapshot")).toBeTruthy();
    });
    // Replay RPC was called with the right session id.
    const replayCall = call.mock.calls.find(
      (c: unknown[]): boolean => c[0] === "engine.replay_session",
    );
    expect(replayCall).toBeDefined();
    expect(replayCall?.[1]).toEqual({ session_id: sessions[0].id });
    // Event count + deck info rendered.
    expect(screen.getByTestId("sessions-replay-event-count").textContent).toContain("7");
    const snapshot = screen.getByTestId("sessions-replay-snapshot");
    expect(snapshot.textContent).toContain("125.50"); // master bpm
    expect(snapshot.textContent).toContain("track-1");
    expect(snapshot.textContent).toContain("Deck B"); // deck B section still renders even when empty
  });

  it("renders empty-state when no sessions persisted yet", async (): Promise<void> => {
    const { client } = makeClient({
      "engine.list_sessions": { sessions: [] },
    });
    render(<Sessions client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId("sessions-empty")).toBeTruthy();
    });
    expect(screen.getByTestId("sessions-empty").textContent).toMatch(
      /no past sessions/i,
    );
  });

  it("each row exposes an export-crowd-pleaser button bound to the session id", async (): Promise<void> => {
    const sessions = [
      makeSummary("20260518T013312Z-eeee", { has_recording: true }),
      makeSummary("20260518T015555Z-ffff", {
        has_recording: false,
        recording_size_bytes: null,
      }),
    ];
    const { client } = makeClient({
      "engine.list_sessions": { sessions },
    });
    render(<Sessions client={client} />);
    await waitFor((): void => {
      expect(
        screen.getByTestId(`sessions-export-btn-${sessions[0].id}`),
      ).toBeTruthy();
    });
    // First row's button is enabled (recording present).
    const enabled = screen.getByTestId(
      `sessions-export-btn-${sessions[0].id}`,
    ) as HTMLButtonElement;
    expect(enabled.disabled).toBe(false);
    // Second row's button is disabled (no recording).
    const disabled = screen.getByTestId(
      `sessions-export-btn-${sessions[1].id}`,
    ) as HTMLButtonElement;
    expect(disabled.disabled).toBe(true);
  });

  it("shows a loading state during fetch and an error banner on RPC failure", async (): Promise<void> => {
    let resolveList: (value: unknown) => void = () => undefined;
    const pending = new Promise<unknown>((resolve) => {
      resolveList = resolve;
    });
    const call = vi.fn<Call>((method: string): Promise<unknown> => {
      if (method === "engine.list_sessions") return pending;
      return Promise.reject(new Error(`unmocked: ${method}`));
    });
    const client = { call } as unknown as JsonRpcWS;
    render(<Sessions client={client} />);
    // Loading state visible while the promise is in flight.
    await waitFor((): void => {
      expect(screen.getByTestId("sessions-loading")).toBeTruthy();
    });
    resolveList({ sessions: [] });
    await waitFor((): void => {
      expect(screen.queryByTestId("sessions-loading")).toBeNull();
    });

    // Now mount a fresh component with a failing call.
    cleanup();
    __resetSessionsStore();
    const failingCall = vi
      .fn<Call>()
      .mockRejectedValue(new Error("engine offline"));
    const failingClient = { call: failingCall } as unknown as JsonRpcWS;
    render(<Sessions client={failingClient} />);
    await waitFor((): void => {
      expect(screen.getByTestId("sessions-error")).toBeTruthy();
    });
    expect(screen.getByTestId("sessions-error").textContent).toContain(
      "engine offline",
    );
  });
});
