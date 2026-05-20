// presets.test.ts — preset store CRUD bridge tests.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { JsonRpcWS } from "../ws/client";
import {
  __getPresetsSnapshot,
  __resetPresetsStore,
  deletePreset,
  fetchPresets,
  loadPreset,
  refetchPresets,
  savePreset,
  type Preset,
} from "./presets";

const makeClient = (
  responses: Record<string, unknown>,
): { client: JsonRpcWS; call: ReturnType<typeof vi.fn> } => {
  const call = vi.fn(
    (method: string): Promise<unknown> => {
      if (method in responses) return Promise.resolve(responses[method]);
      return Promise.reject(new Error(`unmocked method: ${method}`));
    },
  );
  return { client: { call } as unknown as JsonRpcWS, call };
};

const samplePreset = (id = 1, name = "scene"): Preset => ({
  id,
  name,
  created_at: "2026-05-17T22:00:00Z",
  deck_a: {
    effects: [
      { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
      { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
      { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
    ],
    eq_low_db: 0,
    eq_mid_db: 0,
    eq_high_db: 0,
    pitch_semitones: 0,
    tempo_ratio: 1,
  },
  deck_b: {
    effects: [
      { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
      { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
      { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
    ],
    eq_low_db: 0,
    eq_mid_db: 0,
    eq_high_db: 0,
    pitch_semitones: 0,
    tempo_ratio: 1,
  },
  crossfader_curve: "Linear",
});

describe("presets store", () => {
  beforeEach((): void => {
    __resetPresetsStore();
  });
  afterEach((): void => {
    __resetPresetsStore();
  });

  it("fetchPresets populates the cache from the wire response", async (): Promise<void> => {
    const { client, call } = makeClient({
      "presets.list": {
        presets: [
          { id: 1, name: "a", created_at: "2026-05-17T10:00:00Z" },
          { id: 2, name: "b", created_at: "2026-05-17T11:00:00Z" },
        ],
      },
    });
    const result = await fetchPresets(client);
    expect(call).toHaveBeenCalledWith("presets.list", {});
    expect(result.presets).toHaveLength(2);
    expect(result.presets[0].name).toBe("a");
    expect(result.error).toBeNull();
  });

  it("fetchPresets records the error on rejection", async (): Promise<void> => {
    const call = vi.fn(
      (): Promise<unknown> => Promise.reject(new Error("transport down")),
    );
    const client = { call } as unknown as JsonRpcWS;
    const result = await fetchPresets(client);
    expect(result.error).toBe("transport down");
    expect(result.loaded).toBe(true);
    expect(result.presets).toEqual([]);
  });

  it("savePreset returns the saved preset and prepends it to the cache", async (): Promise<void> => {
    const saved = samplePreset(7, "warmup");
    const { client } = makeClient({
      "presets.save": { preset_id: 7, preset: saved },
    });
    const result = await savePreset(client, {
      name: "warmup",
      deck_a: saved.deck_a,
      deck_b: saved.deck_b,
      crossfader_curve: "Linear",
    });
    expect(result?.id).toBe(7);
    const snapshot = __getPresetsSnapshot();
    expect(snapshot.presets[0].id).toBe(7);
  });

  it("savePreset surfaces the error string on duplicate-name failure", async (): Promise<void> => {
    const call = vi.fn(
      (): Promise<unknown> =>
        Promise.reject(new Error("preset name already exists")),
    );
    const client = { call } as unknown as JsonRpcWS;
    const result = await savePreset(client, {
      name: "dupe",
      deck_a: samplePreset().deck_a,
      deck_b: samplePreset().deck_b,
      crossfader_curve: "Linear",
    });
    expect(result).toBeNull();
    const snapshot = __getPresetsSnapshot();
    expect(snapshot.error).toBe("preset name already exists");
  });

  it("loadPreset returns the full preset body", async (): Promise<void> => {
    const preset = samplePreset(3, "deep");
    const { client } = makeClient({
      "presets.load": { preset },
    });
    const result = await loadPreset(client, 3);
    expect(result?.name).toBe("deep");
    expect(result?.crossfader_curve).toBe("Linear");
  });

  it("loadPreset returns null on RPC failure", async (): Promise<void> => {
    const call = vi.fn(
      (): Promise<unknown> => Promise.reject(new Error("not found")),
    );
    const client = { call } as unknown as JsonRpcWS;
    const result = await loadPreset(client, 999);
    expect(result).toBeNull();
  });

  it("fetchPresets short-circuits when already loaded (no refetch on remount)", async (): Promise<void> => {
    const { client, call } = makeClient({
      "presets.list": {
        presets: [{ id: 1, name: "a", created_at: "2026-05-17T10:00:00Z" }],
      },
    });
    await fetchPresets(client);
    await fetchPresets(client);
    await fetchPresets(client);
    expect(call).toHaveBeenCalledTimes(1);
  });

  it("in-flight presets.list discards when a save lands first (no row erase)", async (): Promise<void> => {
    // Race scenario from Codex #231 R1 P1: WS reconnect fires
    // refetchPresets, the user saves a preset before the list
    // response returns, the stale list arrives and would normally
    // overwrite the cache and erase the just-saved row.
    let resolveList: ((v: unknown) => void) | null = null;
    const listPromise = new Promise<unknown>((res): void => {
      resolveList = res;
    });
    const call = vi.fn((method: string, params?: unknown): Promise<unknown> => {
      if (method === "presets.list") return listPromise;
      if (method === "presets.save") {
        const name = (params as { name: string }).name;
        return Promise.resolve({
          preset: {
            id: 99,
            name,
            created_at: "2026-05-17T12:00:00Z",
            deck_a: {
              effects: [],
              eq_low_db: 0,
              eq_mid_db: 0,
              eq_high_db: 0,
              pitch_semitones: 0,
              tempo_ratio: 1,
            },
            deck_b: {
              effects: [],
              eq_low_db: 0,
              eq_mid_db: 0,
              eq_high_db: 0,
              pitch_semitones: 0,
              tempo_ratio: 1,
            },
            crossfader_curve: "Linear",
          },
        });
      }
      return Promise.reject(new Error(`unmocked ${method}`));
    });
    const client = { call } as unknown as JsonRpcWS;

    // Start the refetch (reconnect path) — RPC is pending.
    const listInFlight = refetchPresets(client);
    // User saves while the list is still pending.
    await savePreset(client, {
      name: "new",
      deck_a: {
        effects: [],
        eq_low_db: 0,
        eq_mid_db: 0,
        eq_high_db: 0,
        pitch_semitones: 0,
        tempo_ratio: 1,
      },
      deck_b: {
        effects: [],
        eq_low_db: 0,
        eq_mid_db: 0,
        eq_high_db: 0,
        pitch_semitones: 0,
        tempo_ratio: 1,
      },
      crossfader_curve: "Linear",
    });
    expect(__getPresetsSnapshot().presets.map((p) => p.id)).toEqual([99]);
    // Now the stale list response lands — must NOT erase id=99.
    resolveList!({
      presets: [{ id: 1, name: "stale", created_at: "2026-05-17T10:00:00Z" }],
    });
    await listInFlight;
    const after = __getPresetsSnapshot();
    expect(after.presets.map((p) => p.id)).toEqual([99]);
    expect(after.loading).toBe(false);
  });

  it("refetchPresets forces a fresh presets.list call even when loaded", async (): Promise<void> => {
    let nthCall = 0;
    const call = vi.fn(async (): Promise<unknown> => {
      nthCall += 1;
      return {
        presets:
          nthCall === 1
            ? [{ id: 1, name: "a", created_at: "2026-05-17T10:00:00Z" }]
            : [
                { id: 1, name: "a", created_at: "2026-05-17T10:00:00Z" },
                { id: 2, name: "b-renamed", created_at: "2026-05-17T11:00:00Z" },
              ],
      };
    });
    const client = { call } as unknown as JsonRpcWS;
    const first = await fetchPresets(client);
    expect(first.presets).toHaveLength(1);
    const second = await refetchPresets(client);
    expect(second.presets).toHaveLength(2);
    expect(second.presets[1]?.name).toBe("b-renamed");
    expect(call).toHaveBeenCalledTimes(2);
  });

  it("deletePreset removes the row from the local cache", async (): Promise<void> => {
    const { client } = makeClient({
      "presets.list": {
        presets: [
          { id: 1, name: "a", created_at: "2026-05-17T10:00:00Z" },
          { id: 2, name: "b", created_at: "2026-05-17T11:00:00Z" },
        ],
      },
      "presets.delete": { ok: true, deleted: true },
    });
    const initial = await fetchPresets(client);
    expect(initial.presets.map((p) => p.id)).toEqual([1, 2]);
    const ok = await deletePreset(client, 1);
    expect(ok).toBe(true);
    const snapshot = __getPresetsSnapshot();
    expect(snapshot.presets.map((p) => p.id)).toEqual([2]);
  });
});
