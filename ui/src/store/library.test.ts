// library.test.ts — fetch/search/error handling for the library store.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { JsonRpcWS } from "../ws/client";
import {
  __getLibrarySnapshot,
  __resetLibraryStore,
  __setLibraryTracks,
  fetchLibrary,
  refetchLibrary,
  searchLibrary,
  setHotCues,
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

  it("fetchLibrary short-circuits when already loaded (no remount thrash)", async (): Promise<void> => {
    const call = vi.fn().mockResolvedValue({
      tracks: [makeTrack("t1")],
      total: 1,
    });
    const client = { call } as unknown as JsonRpcWS;
    await fetchLibrary(client);
    await fetchLibrary(client);
    await fetchLibrary(client);
    expect(call).toHaveBeenCalledTimes(1);
  });

  it("setHotCues splices the updated row into the cache (closes #237)", async (): Promise<void> => {
    __setLibraryTracks([makeTrack("t1"), makeTrack("t2")], 2);
    const updated: LibraryTrack = {
      ...makeTrack("t1"),
      hot_cues: [0, 1500, 3000, null, null, null, null, null],
    };
    const call = vi.fn().mockResolvedValue({ track: updated });
    const client = { call } as unknown as JsonRpcWS;
    const result = await setHotCues(client, "t1", updated.hot_cues);
    expect(result?.hot_cues).toEqual(updated.hot_cues);
    const snap = __getLibrarySnapshot();
    const t1 = snap.tracks.find((t) => t.id === "t1");
    expect(t1?.hot_cues).toEqual(updated.hot_cues);
    // t2 untouched.
    const t2 = snap.tracks.find((t) => t.id === "t2");
    expect(t2?.hot_cues).toEqual(makeTrack("t2").hot_cues);
  });

  it("in-flight list_tracks discards when setHotCues lands first (Codex #238 R1)", async (): Promise<void> => {
    __setLibraryTracks([makeTrack("t1"), makeTrack("t2")], 2);
    // The pending list_tracks would echo the OLD hot_cues (server
    // queried before the setHotCues persist). It must NOT clobber
    // the spliced row.
    let resolveList: ((v: unknown) => void) | null = null;
    const listPromise = new Promise<unknown>((res): void => {
      resolveList = res;
    });
    const newCues: ReadonlyArray<number | null> = [
      0, 1500, 3000, null, null, null, null, null,
    ];
    const updatedT1: LibraryTrack = { ...makeTrack("t1"), hot_cues: newCues };
    const call = vi.fn((method: string): Promise<unknown> => {
      if (method === "library.list_tracks") return listPromise;
      if (method === "library.set_hot_cues") {
        return Promise.resolve({ track: updatedT1 });
      }
      return Promise.reject(new Error(`unmocked ${method}`));
    });
    const client = { call } as unknown as JsonRpcWS;

    // Start the refetch (reconnect path) — RPC pending.
    const listInFlight = refetchLibrary(client);
    // User edits hot cues while the list is still pending.
    await setHotCues(client, "t1", newCues);
    expect(__getLibrarySnapshot().tracks.find((t) => t.id === "t1")?.hot_cues)
      .toEqual(newCues);
    // Now the stale list response lands — must NOT erase the new cues.
    resolveList!({
      tracks: [makeTrack("t1"), makeTrack("t2")],
      total: 2,
    });
    await listInFlight;
    const after = __getLibrarySnapshot();
    expect(after.tracks.find((t) => t.id === "t1")?.hot_cues).toEqual(newCues);
    expect(after.loading).toBe(false);
  });

  it("setHotCues on a track not in the cache is a defensive no-op", async (): Promise<void> => {
    __setLibraryTracks([makeTrack("t1")], 1);
    const before = __getLibrarySnapshot();
    const ghost: LibraryTrack = {
      ...makeTrack("ghost"),
      hot_cues: [0, null, null, null, null, null, null, null],
    };
    const call = vi.fn().mockResolvedValue({ track: ghost });
    const client = { call } as unknown as JsonRpcWS;
    const result = await setHotCues(client, "ghost", ghost.hot_cues);
    expect(result?.id).toBe("ghost");
    // Cache unchanged — ghost wasn't there, don't insert.
    expect(__getLibrarySnapshot().tracks).toEqual(before.tracks);
  });

  it("refetchLibrary forces a fresh list_tracks call even when loaded", async (): Promise<void> => {
    let nthCall = 0;
    const call = vi.fn(async (): Promise<unknown> => {
      nthCall += 1;
      return {
        tracks:
          nthCall === 1
            ? [makeTrack("t1")]
            : [makeTrack("t1"), makeTrack("t2")],
        total: nthCall === 1 ? 1 : 2,
      };
    });
    const client = { call } as unknown as JsonRpcWS;
    const first = await fetchLibrary(client);
    expect(first.tracks).toHaveLength(1);
    const second = await refetchLibrary(client);
    expect(second.tracks).toHaveLength(2);
    expect(call).toHaveBeenCalledTimes(2);
  });
});
