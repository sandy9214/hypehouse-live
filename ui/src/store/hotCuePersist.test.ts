// hotCuePersist.test.ts — debounced library.set_hot_cues bridge tests.
//
// Covers:
//   * `noteLoadedTrack` binds a deck to a library track id;
//   * `recordHotCueSet` no-ops when no track is bound;
//   * `recordHotCueSet` fires `library.set_hot_cues` exactly once
//     after `debounceMs` of idle (coalescing burst sets);
//   * `recordHotCueSet` cancels the prior debounce when a new track
//     is loaded mid-flight (a stale write would corrupt the new row);
//   * `flushHotCuePersist` flushes immediately (no waiting on timer).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { JsonRpcWS } from "../ws/client";
import {
  __getLoadedTrack,
  __resetHotCuePersist,
  flushHotCuePersist,
  noteLoadedTrack,
  recordHotCueSet,
} from "./hotCuePersist";

const makeClient = (): {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
} => {
  const call = vi.fn().mockResolvedValue({
    track: {
      id: "fake",
      path: "/m/fake.mp3",
      bpm: 124,
      camelot_key: "8B",
      energy: 0.2,
      duration_s: 200,
      beat_grid_anchor_ms: 0,
      beat_period_ms: 483.87,
      downbeats_ms: [],
      hot_cues: [null, null, null, null, null, null, null, null],
    },
  });
  return { client: { call } as unknown as JsonRpcWS, call };
};

const calls = (
  call: ReturnType<typeof vi.fn>,
  method: string,
): unknown[][] =>
  call.mock.calls.filter((c: unknown[]): boolean => c[0] === method);

describe("hotCuePersist", () => {
  beforeEach((): void => {
    vi.useFakeTimers();
    __resetHotCuePersist();
  });
  afterEach((): void => {
    vi.useRealTimers();
    __resetHotCuePersist();
  });

  it("noteLoadedTrack records the deck→track binding", (): void => {
    noteLoadedTrack("A", "song-7");
    expect(__getLoadedTrack("A")).toBe("song-7");
    expect(__getLoadedTrack("B")).toBeUndefined();
  });

  it("recordHotCueSet no-ops when no track is bound to the deck", (): void => {
    const { client, call } = makeClient();
    const cues = [100, null, null, null, null, null, null, null];
    recordHotCueSet(client, "A", cues, 100);
    vi.advanceTimersByTime(500);
    expect(calls(call, "library.set_hot_cues")).toHaveLength(0);
  });

  it("recordHotCueSet fires library.set_hot_cues after debounce", (): void => {
    const { client, call } = makeClient();
    noteLoadedTrack("A", "song-7");
    const cues = [100, null, null, null, null, null, null, null];
    recordHotCueSet(client, "A", cues, 500);
    // Mid-debounce: no call yet.
    vi.advanceTimersByTime(499);
    expect(calls(call, "library.set_hot_cues")).toHaveLength(0);
    // After the debounce idle window: exactly one call with the
    // expected payload.
    vi.advanceTimersByTime(2);
    const matching = calls(call, "library.set_hot_cues");
    expect(matching).toHaveLength(1);
    expect(matching[0][1]).toEqual({
      track_id: "song-7",
      hot_cues: cues,
    });
  });

  it("recordHotCueSet coalesces a burst of sets into one flush", (): void => {
    const { client, call } = makeClient();
    noteLoadedTrack("A", "song-7");
    // Three rapid sets within the debounce window — only the last
    // snapshot should land on the wire.
    recordHotCueSet(client, "A", [1, null, null, null, null, null, null, null], 500);
    vi.advanceTimersByTime(100);
    recordHotCueSet(client, "A", [1, 2, null, null, null, null, null, null], 500);
    vi.advanceTimersByTime(100);
    recordHotCueSet(client, "A", [1, 2, 3, null, null, null, null, null], 500);
    vi.advanceTimersByTime(600);
    const matching = calls(call, "library.set_hot_cues");
    expect(matching).toHaveLength(1);
    expect(
      (matching[0][1] as { hot_cues: ReadonlyArray<number | null> }).hot_cues,
    ).toEqual([1, 2, 3, null, null, null, null, null]);
  });

  it("loading a new track cancels a pending flush for that deck", (): void => {
    const { client, call } = makeClient();
    noteLoadedTrack("A", "song-old");
    recordHotCueSet(
      client,
      "A",
      [42, null, null, null, null, null, null, null],
      500,
    );
    // Mid-debounce, user loads a different track.
    noteLoadedTrack("A", "song-new");
    vi.advanceTimersByTime(600);
    // No flush — the queued write was for the prior track and would
    // have stomped the new track's row.
    expect(calls(call, "library.set_hot_cues")).toHaveLength(0);
  });

  it("flushHotCuePersist drains the queued write immediately", (): void => {
    const { client, call } = makeClient();
    noteLoadedTrack("B", "song-7");
    recordHotCueSet(
      client,
      "B",
      [null, null, null, null, null, null, null, 99_000],
      500,
    );
    expect(calls(call, "library.set_hot_cues")).toHaveLength(0);
    const flushed = flushHotCuePersist(client, "B");
    expect(flushed).toBe("song-7");
    expect(calls(call, "library.set_hot_cues")).toHaveLength(1);
  });
});
