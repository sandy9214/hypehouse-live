// PlaylistQueue.test.tsx — render + interaction assertions.
//
// Each test seeds the store via the test-only `__setPlaylistEntries`
// hook so the panel doesn't fire its first-mount `playlist.list` RPC
// against the mock client (would otherwise count as an extra call
// that the assertion has to filter out).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { PlaylistQueue } from "./PlaylistQueue";
import type { JsonRpcWS } from "../ws/client";
import {
  __resetPlaylistStore,
  __setPlaylistEntries,
  type PlaylistEntry,
} from "../store/playlist";
import type { LibraryTrack } from "../store/library";

const makeTrack = (id: string): LibraryTrack => ({
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
});

const makeEntry = (id: string, position: number): PlaylistEntry => ({
  track_id: id,
  position,
  added_at: "2026-05-18T00:00:00+00:00",
  track: makeTrack(id),
});

type CallFn = (method: string, params?: unknown) => Promise<unknown>;

const makeClient = (): {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
} => {
  const call = vi.fn<CallFn>((_method: string): Promise<unknown> => {
    // Default responses for the mutation RPCs — return the list shape
    // so the store's de-serialization passes.
    return Promise.resolve({ entries: [], ok: true, entry: null });
  });
  return { client: { call } as unknown as JsonRpcWS, call };
};

describe("PlaylistQueue", () => {
  beforeEach((): void => {
    __resetPlaylistStore();
  });

  afterEach((): void => {
    cleanup();
  });

  it("renders the empty-state message when no entries", (): void => {
    __setPlaylistEntries([]);
    const { client } = makeClient();
    render(<PlaylistQueue client={client} />);
    expect(screen.getByTestId("playlist-empty")).toBeTruthy();
    expect(screen.getByTestId("playlist-count").textContent).toContain(
      "0 tracks",
    );
  });

  it("renders one row per entry in position order", (): void => {
    __setPlaylistEntries([
      makeEntry("a", 0),
      makeEntry("b", 1),
      makeEntry("c", 2),
    ]);
    const { client } = makeClient();
    render(<PlaylistQueue client={client} />);
    expect(screen.getByTestId("playlist-row-a")).toBeTruthy();
    expect(screen.getByTestId("playlist-row-b")).toBeTruthy();
    expect(screen.getByTestId("playlist-row-c")).toBeTruthy();
    expect(screen.getByTestId("playlist-count").textContent).toContain(
      "3 tracks",
    );
  });

  it("clicking ↓ on a middle row emits playlist.reorder with new_position+1", async (): Promise<void> => {
    __setPlaylistEntries([
      makeEntry("a", 0),
      makeEntry("b", 1),
      makeEntry("c", 2),
    ]);
    const { client, call } = makeClient();
    render(<PlaylistQueue client={client} />);
    fireEvent.click(screen.getByTestId("playlist-down-b"));
    await Promise.resolve();
    expect(call).toHaveBeenCalledWith("playlist.reorder", {
      track_id: "b",
      new_position: 2,
    });
  });

  it("up button is disabled on first row + down disabled on last row", (): void => {
    __setPlaylistEntries([makeEntry("a", 0), makeEntry("b", 1)]);
    const { client } = makeClient();
    render(<PlaylistQueue client={client} />);
    const upFirst = screen.getByTestId(
      "playlist-up-a",
    ) as HTMLButtonElement;
    expect(upFirst.disabled).toBe(true);
    const downLast = screen.getByTestId(
      "playlist-down-b",
    ) as HTMLButtonElement;
    expect(downLast.disabled).toBe(true);
  });

  it("X button emits playlist.remove with the entry's track_id", async (): Promise<void> => {
    __setPlaylistEntries([makeEntry("a", 0), makeEntry("b", 1)]);
    const { client, call } = makeClient();
    render(<PlaylistQueue client={client} />);
    fireEvent.click(screen.getByTestId("playlist-remove-a"));
    await Promise.resolve();
    expect(call).toHaveBeenCalledWith("playlist.remove", {
      track_id: "a",
    });
  });

  it("Clear button emits playlist.clear (only shown when non-empty)", async (): Promise<void> => {
    __setPlaylistEntries([makeEntry("a", 0)]);
    const { client, call } = makeClient();
    render(<PlaylistQueue client={client} />);
    fireEvent.click(screen.getByTestId("playlist-clear"));
    await Promise.resolve();
    expect(call).toHaveBeenCalledWith("playlist.clear", {});
  });

  it("dropping a library-track payload enqueues by id", async (): Promise<void> => {
    __setPlaylistEntries([]);
    const { client, call } = makeClient();
    render(<PlaylistQueue client={client} />);
    const panel = screen.getByTestId("playlist-panel");
    const payload = JSON.stringify({ id: "dropped-track" });
    // jsdom's DataTransfer is minimal; the component reads two APIs:
    // `types.includes(...)` and `getData(...)`. Stub both.
    const dataTransfer: unknown = {
      types: ["application/x-hypehouse-track"],
      getData: (mime: string): string =>
        mime === "application/x-hypehouse-track" ? payload : "",
      dropEffect: "copy",
    };
    fireEvent.dragOver(panel, { dataTransfer });
    fireEvent.drop(panel, { dataTransfer });
    await Promise.resolve();
    expect(call).toHaveBeenCalledWith("playlist.enqueue", {
      track_id: "dropped-track",
    });
  });

  it("renders 'missing' badge when an entry's track is null", (): void => {
    __setPlaylistEntries([
      { ...makeEntry("ghost", 0), track: null },
    ]);
    const { client } = makeClient();
    render(<PlaylistQueue client={client} />);
    const row = screen.getByTestId("playlist-row-ghost");
    expect(row.textContent).toContain("missing");
  });
});
