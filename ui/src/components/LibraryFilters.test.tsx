// LibraryFilters.test.tsx — chip bar UX + store wiring.
//
// Covers:
//   1. BPM range slider drag updates filter state.
//   2. Compatible-with picker emits track_id into filter state.
//   3. Active chips render + clearable.
//   4. Filter state survives unmount/remount (= "navigation").
//   5. Apply filter triggers library.search_tracks with the chip args.

import { act } from "react";
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
import { Library } from "./Library";
import { LibraryFilters } from "./LibraryFilters";
import type { JsonRpcWS } from "../ws/client";
import {
  __resetLibraryFilters,
  __resetLibraryStore,
  EMPTY_FILTERS,
  setLibraryFilters,
  type LibraryTrack,
} from "../store/library";

const makeTrack = (
  id: string,
  extra: Partial<LibraryTrack> = {},
): LibraryTrack => ({
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

const makeClient = (
  responses: Record<string, unknown> | ((method: string, params?: unknown) => unknown),
): { client: JsonRpcWS; call: ReturnType<typeof vi.fn> } => {
  const call = vi.fn<Call>(
    (method: string, params?: unknown): Promise<unknown> => {
      if (typeof responses === "function") {
        const r = responses(method, params);
        if (r === undefined) {
          return Promise.reject(new Error(`unmocked: ${method}`));
        }
        return Promise.resolve(r);
      }
      if (method in responses) return Promise.resolve(responses[method]);
      return Promise.reject(new Error(`unmocked method: ${method}`));
    },
  );
  return { client: { call } as unknown as JsonRpcWS, call };
};

describe("LibraryFilters", () => {
  beforeEach((): void => {
    __resetLibraryStore();
    __resetLibraryFilters();
  });
  afterEach((): void => {
    cleanup();
    __resetLibraryStore();
    __resetLibraryFilters();
    vi.useRealTimers();
  });

  it("BPM range slider drag updates filter state and chip", (): void => {
    const { client } = makeClient({});
    render(<LibraryFilters client={client} />);
    // No chip rendered before any change.
    expect(screen.queryByTestId("library-filter-chip-bpm")).toBeNull();

    // Drag the min handle to 124. Once non-default, the chip renders.
    fireEvent.change(screen.getByTestId("library-filter-bpm-min"), {
      target: { value: "124" },
    });
    expect(screen.getByTestId("library-filter-chip-bpm")).toBeTruthy();
    expect(screen.getByTestId("library-filter-bpm-lo").textContent).toBe("124");

    // Drag max handle down to 130 — chip text updates inclusive.
    fireEvent.change(screen.getByTestId("library-filter-bpm-max"), {
      target: { value: "130" },
    });
    expect(screen.getByTestId("library-filter-chip-bpm").textContent).toContain(
      "BPM 124-130",
    );
  });

  it("Compatible-with picker selects a track_id into filter state", async (): Promise<void> => {
    const reference = makeTrack("kanye-stronger", { camelot_key: "8B" });
    const { client, call } = makeClient({
      "library.search_tracks": {
        tracks: [reference],
        query: "kan",
        limit: 8,
      },
    });
    // Picker debounce 0 -> microtask resolution lands the dropdown
    // without needing fake timers (which deadlock against
    // `findByTestId`'s waitFor polling).
    render(<LibraryFilters client={client} pickerDebounceMs={0} />);

    fireEvent.change(screen.getByTestId("library-filter-compat-input"), {
      target: { value: "kan" },
    });
    // Wait for library.search_tracks to fire + resolve, then dropdown
    // renders.
    const option = await screen.findByTestId(
      "library-filter-compat-option-kanye-stronger",
    );
    expect(
      call.mock.calls.filter(
        (c: unknown[]): boolean => c[0] === "library.search_tracks",
      ).length,
    ).toBeGreaterThanOrEqual(1);
    fireEvent.click(option);
    expect(screen.getByTestId("library-filter-chip-compat")).toBeTruthy();
    expect(
      screen.getByTestId("library-filter-chip-compat").textContent,
    ).toContain("kanye-stronger");
  });

  it("active chips are individually clearable and 'clear all' resets", (): void => {
    const { client } = makeClient({});
    // Seed both filters via the store helper — same effect as user
    // interaction but exercises the chip-only render path.
    setLibraryFilters({
      bpmMin: 124,
      bpmMax: 130,
      compatibleWithTrackId: "alpha",
      pendingSyncOnly: false,
    });
    render(<LibraryFilters client={client} />);
    expect(screen.getByTestId("library-filter-chip-bpm")).toBeTruthy();
    expect(screen.getByTestId("library-filter-chip-compat")).toBeTruthy();

    // Clear BPM chip only — compat chip survives.
    fireEvent.click(screen.getByTestId("library-filter-chip-bpm-clear"));
    expect(screen.queryByTestId("library-filter-chip-bpm")).toBeNull();
    expect(screen.getByTestId("library-filter-chip-compat")).toBeTruthy();

    // Click "clear all" — compat chip disappears too.
    fireEvent.click(screen.getByTestId("library-filter-clear-all"));
    expect(screen.queryByTestId("library-filter-chip-compat")).toBeNull();
  });

  it("filter state survives component remount (= navigation)", (): void => {
    const { client } = makeClient({});
    setLibraryFilters({
      bpmMin: 120,
      bpmMax: 128,
      compatibleWithTrackId: "ref-track",
      pendingSyncOnly: false,
    });
    const { unmount } = render(<LibraryFilters client={client} />);
    expect(screen.getByTestId("library-filter-chip-bpm")).toBeTruthy();
    expect(screen.getByTestId("library-filter-chip-compat")).toBeTruthy();

    // Unmount + remount — store-backed filter slice survives because
    // it lives at module scope (mirrors the tracks cache pattern).
    unmount();
    render(<LibraryFilters client={client} />);
    expect(screen.getByTestId("library-filter-chip-bpm").textContent).toContain(
      "BPM 120-128",
    );
    expect(
      screen.getByTestId("library-filter-chip-compat").textContent,
    ).toContain("ref-track");
  });

  it("toggling a filter chip re-runs library.search_tracks with the chip args", async (): Promise<void> => {
    vi.useFakeTimers();
    const tracks = [
      makeTrack("alpha", { bpm: 124, camelot_key: "8B" }),
      makeTrack("charlie", { bpm: 128, camelot_key: "9B" }),
    ];
    const { client, call } = makeClient(
      (method: string): unknown => {
        if (method === "library.list_tracks") {
          return { tracks, total: 2, limit: 100, offset: 0 };
        }
        if (method === "library.search_tracks") {
          return { tracks: [tracks[1]], query: "", limit: 100 };
        }
        return undefined;
      },
    );
    render(<Library client={client} searchDebounceMs={50} />);
    // Wait for list_tracks to land.
    await act(async (): Promise<void> => {
      await vi.advanceTimersByTimeAsync(1);
    });

    // Set BPM 125-130 via the slider -> debounce -> search fires
    // with bpm_min + bpm_max in params.
    fireEvent.change(screen.getByTestId("library-filter-bpm-min"), {
      target: { value: "125" },
    });
    fireEvent.change(screen.getByTestId("library-filter-bpm-max"), {
      target: { value: "130" },
    });
    await act(async (): Promise<void> => {
      await vi.advanceTimersByTimeAsync(60);
    });

    const searchCalls = call.mock.calls.filter(
      (c: unknown[]): boolean => c[0] === "library.search_tracks",
    );
    expect(searchCalls.length).toBeGreaterThan(0);
    const lastParams = searchCalls[searchCalls.length - 1]?.[1] as {
      bpm_min?: number;
      bpm_max?: number;
    };
    expect(lastParams.bpm_min).toBe(125);
    expect(lastParams.bpm_max).toBe(130);
  });

  it("compatible_with_track_id forwarded into search params on chip set", async (): Promise<void> => {
    vi.useFakeTimers();
    const tracks = [
      makeTrack("alpha", { bpm: 124, camelot_key: "8B" }),
      makeTrack("bravo", { bpm: 124, camelot_key: "8B" }),
    ];
    const { client, call } = makeClient(
      (method: string): unknown => {
        if (method === "library.list_tracks") {
          return { tracks, total: 2, limit: 100, offset: 0 };
        }
        if (method === "library.search_tracks") {
          return { tracks: [tracks[1]], query: "", limit: 100 };
        }
        return undefined;
      },
    );
    render(<Library client={client} searchDebounceMs={20} />);
    await act(async (): Promise<void> => {
      await vi.advanceTimersByTimeAsync(1);
    });

    // Programmatic seed — equivalent to clicking a picker option.
    act((): void => {
      setLibraryFilters({
        ...EMPTY_FILTERS,
        compatibleWithTrackId: "alpha",
      });
    });
    await act(async (): Promise<void> => {
      await vi.advanceTimersByTimeAsync(30);
    });

    const searchCall = call.mock.calls.find(
      (c: unknown[]): boolean =>
        c[0] === "library.search_tracks" &&
        (c[1] as { compatible_with_track_id?: string })
          .compatible_with_track_id === "alpha",
    );
    expect(searchCall).toBeDefined();
  });
});

describe("LibraryFilters — wired into Library", () => {
  beforeEach((): void => {
    __resetLibraryStore();
    __resetLibraryFilters();
  });
  afterEach((): void => {
    cleanup();
    __resetLibraryStore();
    __resetLibraryFilters();
    vi.useRealTimers();
  });

  it("pending-sync-only toggle drives the filter chip + store state", (): void => {
    const { client } = makeClient({});
    render(<LibraryFilters client={client} />);
    // No chip before the toggle is clicked.
    expect(screen.queryByTestId("library-filter-chip-pending")).toBeNull();
    const toggle = screen.getByTestId(
      "library-filter-pending-toggle",
    ) as HTMLInputElement;
    expect(toggle.checked).toBe(false);
    // Click → chip appears + checkbox shows checked.
    fireEvent.click(toggle);
    expect(toggle.checked).toBe(true);
    expect(screen.getByTestId("library-filter-chip-pending")).toBeTruthy();
    // Click the chip's × to remove the filter — checkbox + chip
    // both unwind.
    fireEvent.click(
      screen.getByTestId("library-filter-chip-pending-clear"),
    );
    expect(toggle.checked).toBe(false);
    expect(screen.queryByTestId("library-filter-chip-pending")).toBeNull();
  });

  it("LibraryFilters is rendered above the track list inside Library", async (): Promise<void> => {
    const { client } = makeClient({
      "library.list_tracks": { tracks: [], total: 0, limit: 100, offset: 0 },
    });
    render(<Library client={client} />);
    await waitFor((): void => {
      expect(screen.getByTestId("library-filters")).toBeTruthy();
    });
  });
});
