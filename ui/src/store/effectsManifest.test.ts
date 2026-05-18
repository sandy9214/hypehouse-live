// effectsManifest.test.ts — fetch on first call, cache thereafter,
// tolerate bad payload shapes without throwing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { JsonRpcWS } from "../ws/client";
import {
  __resetEffectsManifest,
  fetchEffectsManifest,
} from "./effectsManifest";

describe("effectsManifest store", () => {
  beforeEach((): void => {
    __resetEffectsManifest();
  });
  afterEach((): void => {
    __resetEffectsManifest();
  });

  it("fetches and caches the manifest on first call", async (): Promise<void> => {
    const call = vi.fn().mockResolvedValue({
      effects: [
        {
          id: 1,
          name: "filter",
          params: [{ name: "cutoff_hz", min: 20, max: 20000, default: 500 }],
        },
      ],
    });
    const client = { call } as unknown as JsonRpcWS;
    const m1 = await fetchEffectsManifest(client);
    const m2 = await fetchEffectsManifest(client);
    expect(m1.length).toBe(1);
    expect(m1[0]?.name).toBe("filter");
    expect(m2).toBe(m1); // same reference — cached
    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith("engine.list_effects");
  });

  it("returns empty manifest on RPC error and does not throw", async (): Promise<void> => {
    const call = vi.fn().mockRejectedValue(new Error("socket not open"));
    const client = { call } as unknown as JsonRpcWS;
    const m = await fetchEffectsManifest(client);
    expect(m).toEqual([]);
  });

  it("returns empty manifest on malformed payload shape", async (): Promise<void> => {
    const call = vi.fn().mockResolvedValue({ not_effects: "wrong" });
    const client = { call } as unknown as JsonRpcWS;
    const m = await fetchEffectsManifest(client);
    expect(m).toEqual([]);
  });
});
