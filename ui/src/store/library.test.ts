// library.test.ts — fetch/search/error handling for the library store.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { JsonRpcWS } from "../ws/client";
import {
  __resetLibraryStore,
  __setLibraryTracks,
  fetchLibrary,
  searchLibrary,
  type LibraryTrack,
} from "./library";

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

describe("library store", () => {
  beforeEach((): void => {
    __resetLibraryStore();
  });
  afterEach((): void => {
    __resetLibraryStore();
  });

  it("fetchLibrary populates the cache on success", async (): Promise<void> => {
    const tracks = [makeTrack("alpha"), makeTrack("bravo")];
    const call = vi.fn().mockResolvedValue({
      tracks,
      total: 2,
      limit: 100,
      offset: 0,
    });
    const client = { call } as unknown as JsonRpcWS;
    const state = await fetchLibrary(client);
    expect(state.loaded).toBe(true);
    expect(state.tracks.map((t: LibraryTrack): string => t.id)).toEqual([
      "alpha",
      "bravo",
    ]);
    expect(state.total).toBe(2);
    expect(state.error).toBeNull();
    expect(call).toHaveBeenCalledWith("library.list_tracks", {
      limit: 100,
      offset: 0,
    });
  });

  it("fetchLibrary handles RPC error gracefully", async (): Promise<void> => {
    const call = vi.fn().mockRejectedValue(new Error("connection closed"));
    const client = { call } as unknown as JsonRpcWS;
    const state = await fetchLibrary(client);
    expect(state.loaded).toBe(true);
    expect(state.tracks).toEqual([]);
    expect(state.total).toBe(0);
    expect(state.error).toBe("connection closed");
  });

  it("fetchLibrary tolerates unexpected payload shape", async (): Promise<void> => {
    const call = vi.fn().mockResolvedValue({ wrong: "shape" });
    const client = { call } as unknown as JsonRpcWS;
    const state = await fetchLibrary(client);
    expect(state.loaded).toBe(true);
    expect(state.tracks).toEqual([]);
    expect(state.error).toContain("unexpected shape");
  });

  it("fetchLibrary forwards limit + offset for pagination", async (): Promise<void> => {
    const call = vi.fn().mockResolvedValue({
      tracks: [],
      total: 5,
      limit: 2,
      offset: 4,
    });
    const client = { call } as unknown as JsonRpcWS;
    await fetchLibrary(client, { limit: 2, offset: 4 });
    expect(call).toHaveBeenCalledWith("library.list_tracks", {
      limit: 2,
      offset: 4,
    });
  });

  it("__setLibraryTracks seeds without going through RPC", (): void => {
    __setLibraryTracks([makeTrack("seeded")], 1);
    // State is in module-internal cache; re-import via __setLibraryTracks
    // re-fires the subscriber. Snapshot is exposed via useLibrary in the
    // component test — this test asserts the seed path is callable +
    // non-throwing (a regression catch — the symbol used to be exported
    // for prod, accidentally tree-shakable).
    expect(() => __setLibraryTracks([], 0)).not.toThrow();
  });

  it("searchLibrary forwards query + returns rows on success", async (): Promise<void> => {
    const rows = [makeTrack("kanye-stronger")];
    const call = vi.fn().mockResolvedValue({
      tracks: rows,
      query: "stronger",
      limit: 100,
    });
    const client = { call } as unknown as JsonRpcWS;
    const result = await searchLibrary(client, "stronger");
    expect(result).toEqual(rows);
    expect(call).toHaveBeenCalledWith("library.search_tracks", {
      query: "stronger",
      limit: 100,
    });
  });

  it("searchLibrary returns [] on RPC error (quiet failure)", async (): Promise<void> => {
    const call = vi.fn().mockRejectedValue(new Error("network down"));
    const client = { call } as unknown as JsonRpcWS;
    const result = await searchLibrary(client, "anything");
    expect(result).toEqual([]);
  });

  it("searchLibrary returns [] on bad payload shape", async (): Promise<void> => {
    const call = vi.fn().mockResolvedValue({ not: "matching" });
    const client = { call } as unknown as JsonRpcWS;
    const result = await searchLibrary(client, "anything");
    expect(result).toEqual([]);
  });
});
