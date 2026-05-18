// stems.test.ts — coverage for `requestStems`, `fetchStemStatus`,
// `parseStemStatus`, and the `useStemStatus` polling hook.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";
import type { JsonRpcWS } from "../ws/client";
import {
  DEFAULT_STEM_POLL_MS,
  STEM_ORDER,
  fetchStemStatus,
  parseStemStatus,
  requestStems,
  useStemStatus,
} from "./stems";

type Call = ReturnType<typeof vi.fn>;
const makeClient = (responses: Record<string, unknown[]>): {
  client: JsonRpcWS;
  call: Call;
} => {
  // Per-method response queue — pop one per call. Falls back to last
  // entry if the queue is exhausted (useful for "stays ready" cases).
  const queues = new Map<string, unknown[]>();
  for (const [k, v] of Object.entries(responses)) queues.set(k, [...v]);
  const call = vi.fn(
    (method: string): Promise<unknown> => {
      const q = queues.get(method);
      if (!q || q.length === 0) {
        return Promise.reject(new Error(`unmocked: ${method}`));
      }
      const next = q.length === 1 ? q[0] : q.shift();
      return Promise.resolve(next);
    },
  );
  return { client: { call } as unknown as JsonRpcWS, call };
};

describe("STEM_ORDER", () => {
  it("matches the canonical engine ordering", (): void => {
    expect(STEM_ORDER).toEqual(["vocals", "drums", "bass", "other"]);
  });
});

describe("parseStemStatus", () => {
  it("returns null snapshot for non-object input", (): void => {
    expect(parseStemStatus(null)).toEqual({ status: null, paths: null });
    expect(parseStemStatus(42)).toEqual({ status: null, paths: null });
  });

  it("returns ready+paths for a complete payload", (): void => {
    const snap = parseStemStatus({
      track_id: "x",
      status: "ready",
      stems: {
        vocals: "/v.wav",
        drums: "/d.wav",
        bass: "/b.wav",
        other: "/o.wav",
      },
    });
    expect(snap.status).toBe("ready");
    expect(snap.paths).toEqual(["/v.wav", "/d.wav", "/b.wav", "/o.wav"]);
  });

  it("flips ready-with-missing-stems to failed", (): void => {
    const snap = parseStemStatus({
      status: "ready",
      stems: { vocals: "/v.wav" }, // 3 missing
    });
    expect(snap.status).toBe("failed");
    expect(snap.paths).toBeNull();
  });

  it("passes pending/failed through with null paths", (): void => {
    expect(parseStemStatus({ status: "pending", stems: null })).toEqual({
      status: "pending",
      paths: null,
    });
    expect(parseStemStatus({ status: "failed", stems: null })).toEqual({
      status: "failed",
      paths: null,
    });
  });
});

describe("requestStems", () => {
  it("calls library.compute_stems and returns the wire envelope", async (): Promise<void> => {
    const { client, call } = makeClient({
      "library.compute_stems": [
        { track_id: "song-7", status: "pending" },
      ],
    });
    const out = await requestStems(client, "song-7");
    expect(out).toEqual({ status: "pending", track_id: "song-7" });
    expect(call).toHaveBeenCalledWith("library.compute_stems", {
      track_id: "song-7",
    });
  });

  it("normalises unknown status to null", async (): Promise<void> => {
    const { client } = makeClient({
      "library.compute_stems": [{ track_id: "song-7", status: "bogus" }],
    });
    const out = await requestStems(client, "song-7");
    expect(out.status).toBeNull();
  });
});

describe("fetchStemStatus", () => {
  it("returns ready paths on a successful RPC", async (): Promise<void> => {
    const { client } = makeClient({
      "library.get_stems": [
        {
          track_id: "song-7",
          status: "ready",
          stems: {
            vocals: "/v.wav",
            drums: "/d.wav",
            bass: "/b.wav",
            other: "/o.wav",
          },
        },
      ],
    });
    const snap = await fetchStemStatus(client, "song-7");
    expect(snap).toEqual({
      status: "ready",
      paths: ["/v.wav", "/d.wav", "/b.wav", "/o.wav"],
    });
  });

  it("treats RPC failure as pending (transient blip)", async (): Promise<void> => {
    const call = vi.fn().mockRejectedValue(new Error("connection closed"));
    const client = { call } as unknown as JsonRpcWS;
    const snap = await fetchStemStatus(client, "song-7");
    expect(snap).toEqual({ status: "pending", paths: null });
  });
});

describe("useStemStatus", () => {
  beforeEach((): void => {
    vi.useFakeTimers();
  });
  afterEach((): void => {
    vi.useRealTimers();
  });

  it("idle hook (null trackId) does not call library.get_stems", async (): Promise<void> => {
    const { client, call } = makeClient({});
    const { result } = renderHook(() => useStemStatus(client, null));
    // Flush microtasks so a stray fetch promise would have called by now.
    await act(async (): Promise<void> => {
      await Promise.resolve();
    });
    expect(call).not.toHaveBeenCalled();
    expect(result.current).toEqual({ status: null, paths: null });
  });

  it("polls every 2s until status flips to ready", async (): Promise<void> => {
    // First poll returns pending, second poll returns ready.
    const { client, call } = makeClient({
      "library.get_stems": [
        { track_id: "x", status: "pending", stems: null },
        {
          track_id: "x",
          status: "ready",
          stems: {
            vocals: "/v",
            drums: "/d",
            bass: "/b",
            other: "/o",
          },
        },
      ],
    });
    const { result } = renderHook(() => useStemStatus(client, "x"));
    // Initial fetch fires in the effect's tick — flush microtasks.
    await act(async (): Promise<void> => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(call.mock.calls.length).toBeGreaterThanOrEqual(1);
    expect(result.current.status).toBe("pending");
    // Advance the 2s poll timer + drain the resulting microtasks.
    await act(async (): Promise<void> => {
      vi.advanceTimersByTime(DEFAULT_STEM_POLL_MS);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(result.current.status).toBe("ready");
    expect(result.current.paths).toEqual(["/v", "/d", "/b", "/o"]);
    const pollCalls = call.mock.calls.filter(
      (c): boolean => c[0] === "library.get_stems",
    );
    // Exactly 2 polls — one initial, one post-2s. The hook idles after
    // the ready flip.
    expect(pollCalls).toHaveLength(2);
  });

  it("stops polling once status reaches ready", async (): Promise<void> => {
    const { client, call } = makeClient({
      "library.get_stems": [
        {
          status: "ready",
          stems: {
            vocals: "/v",
            drums: "/d",
            bass: "/b",
            other: "/o",
          },
        },
      ],
    });
    renderHook(() => useStemStatus(client, "x"));
    await act(async (): Promise<void> => {
      await Promise.resolve();
      await Promise.resolve();
    });
    const baseline = call.mock.calls.length;
    // Advance through several poll intervals — no new RPCs should fire.
    await act(async (): Promise<void> => {
      vi.advanceTimersByTime(DEFAULT_STEM_POLL_MS * 3);
      await Promise.resolve();
    });
    expect(call.mock.calls.length).toBe(baseline);
  });

  it("uses 2000ms (DEFAULT_STEM_POLL_MS) as the polling cadence", (): void => {
    expect(DEFAULT_STEM_POLL_MS).toBe(2000);
  });
});
