// Library.test.tsx — renders mock tracks, search debounces, click "→ A"
// emits a DeckLoad event with the correct deck + track payload.

import { act } from "react";
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { Library } from "./Library";
import type { JsonRpcWS } from "../ws/client";
import {
  __resetLibraryFilters,
  __resetLibraryStore,
  setLibraryFilters,
  type LibraryTrack,
} from "../store/library";
import {
  __resetPendingPushIds,
  __setPendingPushIds,
} from "../store/sessionInfo";

const makeTrack = (id: string, extra: Partial<LibraryTrack> = {}): LibraryTrack => ({
  id,
  path: `/m/${id}.mp3`,
  bpm: 124.0,
  camelot_key: "8B",
  energy: 0.2,
  duration_s: 200.0,
  beat_grid_anchor_ms: 0,
  beat_period_ms: 60_000.0 / 124.0,
  downbeats_ms: [],
  hot_cues: [null, null, null, null, null, null, null, null],
  ...extra,
});

type Call = (method: string, params?: unknown) => Promise<unknown>;

const makeClient = (responses: Record<string, unknown>): {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
} => {
  const call = vi.fn<Call>(
    (method: string): Promise<unknown> => {
      if (method in responses) return Promise.resolve(responses[method]);
      return Promise.reject(new Error(`unmocked method: ${method}`));
    },
  );
  return { client: { call } as unknown as JsonRpcWS, call };
};

describe("Library", () => {
  beforeEach((): void => {
    __resetLibraryStore();
    __resetLibraryFilters();
    __resetPendingPushIds();
  });
  afterEach((): void => {
    cleanup();
    __resetLibraryStore();
    __resetLibraryFilters();
    __resetPendingPushIds();
    vi.useRealTimers();
  });

  it("renders empty-state when the library has no tracks", async (): Promise<void> => {
    const { client } = makeClient({
      "library.list_tracks": { tracks: [], total: 0, limit: 100, offset: 0 },
    });
    render(<Library client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId("library-empty")).toBeTruthy();
    });
    // CLI seed command is visible.
    expect(
      screen.getByText(/python -m copilot\.library add/),
    ).toBeTruthy();
  });

  it("renders one TrackRow per fetched track", async (): Promise<void> => {
    const tracks = [makeTrack("alpha"), makeTrack("bravo")];
    const { client } = makeClient({
      "library.list_tracks": {
        tracks,
        total: 2,
        limit: 100,
        offset: 0,
      },
    });
    render(<Library client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId("track-row-alpha")).toBeTruthy();
    });
    expect(screen.getByTestId("track-row-bravo")).toBeTruthy();
    expect(screen.getByTestId("library-total").textContent).toContain("2");
  });

  it("clicking '→ A' submits DeckLoad to deck A", async (): Promise<void> => {
    const track = makeTrack("foo", {
      bpm: 128.0,
      beat_grid_anchor_ms: 42,
      downbeats_ms: [0, 1875, 3750],
    });
    const { client, call } = makeClient({
      "library.list_tracks": {
        tracks: [track],
        total: 1,
        limit: 100,
        offset: 0,
      },
      submit_event: undefined,
    });
    render(<Library client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId("track-row-foo")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("load-foo-A"));
    await waitFor((): void => {
      expect(call).toHaveBeenCalledWith("submit_event", {
        DeckLoad: {
          deck: "A",
          track: { id: "foo", path: "/m/foo.mp3" },
          bpm: 128.0,
          beat_grid_anchor_ms: 42,
          downbeats_ms: [0, 1875, 3750],
          // Hot-cue persistence PR: the load button forwards the
          // library's saved cue array onto the DeckLoad event. Fresh
          // tracks come in with 8 nulls.
          hot_cues: [null, null, null, null, null, null, null, null],
        },
      });
    });
  });

  it("clicking '→ B' targets deck B", async (): Promise<void> => {
    const track = makeTrack("bar");
    const { client, call } = makeClient({
      "library.list_tracks": {
        tracks: [track],
        total: 1,
        limit: 100,
        offset: 0,
      },
      submit_event: undefined,
    });
    render(<Library client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId("load-bar-B")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("load-bar-B"));
    await waitFor((): void => {
      const args = call.mock.calls.find(
        (c: unknown[]): boolean => c[0] === "submit_event",
      );
      expect(args).toBeDefined();
      const payload = args?.[1] as { DeckLoad: { deck: string } };
      expect(payload.DeckLoad.deck).toBe("B");
    });
  });

  it("typed search query debounces and dispatches library.search_tracks", async (): Promise<void> => {
    vi.useFakeTimers();
    const tracks = [makeTrack("alpha"), makeTrack("bravo")];
    const { client, call } = makeClient({
      "library.list_tracks": {
        tracks,
        total: 2,
        limit: 100,
        offset: 0,
      },
      "library.search_tracks": {
        tracks: [tracks[1]],
        query: "bra",
        limit: 100,
      },
    });
    render(<Library client={client} searchDebounceMs={250} />);
    // Wait for the initial library.list_tracks fetch to land.
    await act(async (): Promise<void> => {
      await vi.advanceTimersByTimeAsync(1);
    });
    const input = screen.getByTestId("library-search");
    fireEvent.change(input, { target: { value: "bra" } });
    // Before 250ms, search hasn't fired yet.
    expect(
      call.mock.calls.filter(
        (c: unknown[]): boolean => c[0] === "library.search_tracks",
      ).length,
    ).toBe(0);
    await act(async (): Promise<void> => {
      await vi.advanceTimersByTimeAsync(260);
    });
    expect(
      call.mock.calls.filter(
        (c: unknown[]): boolean => c[0] === "library.search_tracks",
      ).length,
    ).toBe(1);
  });

  it("clicking '→ A' forwards saved hot_cues from the library row", async (): Promise<void> => {
    const cues: ReadonlyArray<number | null> = [
      0,
      1500,
      null,
      8000,
      null,
      null,
      60_000,
      null,
    ];
    const track = makeTrack("hot", {
      bpm: 128.0,
      beat_grid_anchor_ms: 0,
      downbeats_ms: [],
      hot_cues: cues,
    });
    const { client, call } = makeClient({
      "library.list_tracks": {
        tracks: [track],
        total: 1,
        limit: 100,
        offset: 0,
      },
      submit_event: undefined,
    });
    render(<Library client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId("track-row-hot")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("load-hot-A"));
    await waitFor((): void => {
      const sent = call.mock.calls.find(
        (c: unknown[]): boolean => c[0] === "submit_event",
      );
      expect(sent).toBeDefined();
      const payload = sent?.[1] as {
        DeckLoad: { hot_cues: ReadonlyArray<number | null> };
      };
      expect(payload.DeckLoad.hot_cues).toEqual(cues);
    });
  });

  it("pendingSyncOnly filter narrows visible rows to the pending set", async (): Promise<void> => {
    const tracks = [
      makeTrack("alpha"),
      makeTrack("bravo"),
      makeTrack("charlie"),
    ];
    const client = { call: vi.fn<Call>(async (m: string) => {
      if (m === "library.list_tracks") {
        return { tracks, total: tracks.length, limit: 100, offset: 0 };
      }
      if (m === "library.list_pending_push") {
        return { ids: ["bravo"] };
      }
      return null;
    }) } as unknown as JsonRpcWS;
    render(<Library client={client} />);
    // All three rows visible before the filter is on.
    await waitFor((): void => {
      for (const id of ["alpha", "bravo", "charlie"]) {
        expect(screen.getByTestId(`track-row-${id}`)).toBeTruthy();
      }
    });
    // Seed the pending-push set directly (the polling refetch can
    // race; this is the deterministic shortcut).
    act((): void => {
      __setPendingPushIds(["bravo"]);
      setLibraryFilters({
        bpmMin: null,
        bpmMax: null,
        compatibleWithTrackId: null,
        pendingSyncOnly: true,
      });
    });
    // Now only bravo renders.
    await waitFor((): void => {
      expect(screen.getByTestId("track-row-bravo")).toBeTruthy();
      expect(screen.queryByTestId("track-row-alpha")).toBeNull();
      expect(screen.queryByTestId("track-row-charlie")).toBeNull();
    });
  });

  it("surfaces error banner when RPC fails", async (): Promise<void> => {
    const call = vi.fn<Call>().mockRejectedValue(new Error("engine offline"));
    const client = { call } as unknown as JsonRpcWS;
    render(<Library client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId("library-error")).toBeTruthy();
    });
    expect(screen.getByTestId("library-error").textContent).toContain(
      "engine offline",
    );
  });
});
