// PresetPanel.test.tsx — save / load / delete flow tests.
//
// Covers:
//   * Save click prompts → calls `presets.save` with the current deck/curve
//     snapshot.
//   * Load click → fetches `presets.load` → dispatches the replay event
//     sequence (EffectClear+EffectAssign+EffectParam+EffectWetDry+
//     EffectEnable per slot, EqAdjust × 3, PitchBend, TempoBend, plus
//     one SetCrossfaderCurve).
//   * Delete click confirms then calls `presets.delete`.
//   * Empty list shows the helper hint.
//   * Save prompt cancel does NOT call presets.save.

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
import { PresetPanel, replayPreset } from "./PresetPanel";
import { __resetPresetsStore } from "../store/presets";
import type { JsonRpcWS } from "../ws/client";
import type { Deck } from "../store/engine";
import type { Preset } from "../store/presets";

const emptyDeck = (id: "A" | "B"): Deck => ({
  id,
  track_title: null,
  bpm: null,
  position_ms: 0,
  playing: false,
  eq_low: 0,
  eq_mid: 0,
  eq_high: 0,
  pitch_semitones: 0,
  tempo_ratio: 1,
  hot_cues: [null, null, null, null, null, null, null, null],
  loop_in_ms: null,
  loop_out_ms: null,
  copilot_enabled: false,
  effects: [
    { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
    { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
    { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
  ],
  stem_gains: [1, 1, 1, 1],
  stem_mode: false,
});

const populatedDeck = (id: "A" | "B"): Deck => ({
  ...emptyDeck(id),
  eq_low: -3,
  eq_mid: 1,
  eq_high: 2,
  pitch_semitones: 0.5,
  tempo_ratio: 1.05,
  effects: [
    {
      effect_id: 1,
      params: { cutoff_hz: 500 },
      wet_dry: 0.7,
      enabled: true,
    },
    { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
    { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
  ],
});

const makeClient = (
  responses: Record<string, unknown>,
): { client: JsonRpcWS; call: ReturnType<typeof vi.fn> } => {
  const call = vi.fn(
    (method: string): Promise<unknown> => {
      if (method in responses) return Promise.resolve(responses[method]);
      // submit_event is fire-and-forget — succeed silently.
      if (method === "submit_event") return Promise.resolve({});
      return Promise.reject(new Error(`unmocked method: ${method}`));
    },
  );
  return { client: { call } as unknown as JsonRpcWS, call };
};

const samplePreset: Preset = {
  id: 5,
  name: "warmup",
  created_at: "2026-05-17T22:00:00Z",
  deck_a: {
    effects: [
      {
        effect_id: 1,
        params: { cutoff_hz: 500 },
        wet_dry: 0.7,
        enabled: true,
      },
      { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
      { effect_id: 0, params: {}, wet_dry: 0.5, enabled: false },
    ],
    eq_low_db: -3,
    eq_mid_db: 1,
    eq_high_db: 2,
    pitch_semitones: 0.5,
    tempo_ratio: 1.05,
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
  crossfader_curve: "Dipped",
};

describe("PresetPanel", () => {
  beforeEach((): void => {
    __resetPresetsStore();
  });
  afterEach((): void => {
    cleanup();
    __resetPresetsStore();
  });

  it("renders the empty-state helper when no presets exist", async (): Promise<void> => {
    const { client } = makeClient({
      "presets.list": { presets: [] },
    });
    render(
      <PresetPanel
        client={client}
        decks={[emptyDeck("A"), emptyDeck("B")]}
        crossfaderCurve="Linear"
      />,
    );
    await waitFor((): void => {
      expect(screen.getByTestId("preset-empty")).toBeTruthy();
    });
  });

  it("Save current button calls presets.save with the snapshot", async (): Promise<void> => {
    const { client, call } = makeClient({
      "presets.list": { presets: [] },
      "presets.save": {
        preset_id: 1,
        preset: { ...samplePreset, id: 1, name: "fresh" },
      },
    });
    render(
      <PresetPanel
        client={client}
        decks={[populatedDeck("A"), emptyDeck("B")]}
        crossfaderCurve="Sharp"
        promptFn={(): string | null => "fresh"}
      />,
    );
    await waitFor((): void => {
      expect(screen.getByTestId("preset-save")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("preset-save"));
    await waitFor((): void => {
      expect(call).toHaveBeenCalledWith(
        "presets.save",
        expect.objectContaining({
          name: "fresh",
          crossfader_curve: "Sharp",
          deck_a: expect.objectContaining({
            eq_low_db: -3,
            pitch_semitones: 0.5,
          }),
        }),
      );
    });
  });

  it("Save prompt cancel does NOT call presets.save", async (): Promise<void> => {
    const { client, call } = makeClient({
      "presets.list": { presets: [] },
    });
    render(
      <PresetPanel
        client={client}
        decks={[emptyDeck("A"), emptyDeck("B")]}
        crossfaderCurve="Linear"
        promptFn={(): string | null => null}
      />,
    );
    await waitFor((): void => {
      expect(screen.getByTestId("preset-save")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("preset-save"));
    await new Promise((r) => setTimeout(r, 0));
    expect(
      call.mock.calls.some((c) => c[0] === "presets.save"),
    ).toBe(false);
  });

  it("Load button replays preset state via submit_event sequence", async (): Promise<void> => {
    const { client, call } = makeClient({
      "presets.list": {
        presets: [
          {
            id: 5,
            name: "warmup",
            created_at: "2026-05-17T22:00:00Z",
          },
        ],
      },
      "presets.load": { preset: samplePreset },
    });
    render(
      <PresetPanel
        client={client}
        decks={[emptyDeck("A"), emptyDeck("B")]}
        crossfaderCurve="Linear"
      />,
    );
    await waitFor((): void => {
      expect(screen.getByTestId("preset-load-5")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("preset-load-5"));
    await waitFor((): void => {
      expect(call).toHaveBeenCalledWith("presets.load", { id: 5 });
    });
    // Wait for the replay sequence to fire fully — SetCrossfaderCurve
    // is the last event, so its presence implies all earlier ones landed.
    await waitFor((): void => {
      expect(
        call.mock.calls.some(
          (c) =>
            c[0] === "submit_event" &&
            (c[1] as { SetCrossfaderCurve?: unknown }).SetCrossfaderCurve !==
              undefined,
        ),
      ).toBe(true);
    });
    const submitCalls = call.mock.calls.filter(
      (c): boolean => c[0] === "submit_event",
    );
    expect(
      submitCalls.some(
        (c) =>
          (c[1] as { EffectAssign?: { effect_id: number } }).EffectAssign
            ?.effect_id === 1,
      ),
    ).toBe(true);
    expect(
      submitCalls.some(
        (c) =>
          (c[1] as { EqAdjust?: { band: string; value_db: number } })
            .EqAdjust?.band === "Low" &&
          (c[1] as { EqAdjust?: { value_db: number } }).EqAdjust?.value_db ===
            -3,
      ),
    ).toBe(true);
    expect(
      submitCalls.some(
        (c) =>
          (c[1] as { SetCrossfaderCurve?: { curve: string } })
            .SetCrossfaderCurve?.curve === "Dipped",
      ),
    ).toBe(true);
  });

  it("Delete button confirms then calls presets.delete", async (): Promise<void> => {
    const { client, call } = makeClient({
      "presets.list": {
        presets: [
          {
            id: 5,
            name: "warmup",
            created_at: "2026-05-17T22:00:00Z",
          },
        ],
      },
      "presets.delete": { ok: true, deleted: true },
    });
    render(
      <PresetPanel
        client={client}
        decks={[emptyDeck("A"), emptyDeck("B")]}
        crossfaderCurve="Linear"
        confirmFn={(): boolean => true}
      />,
    );
    await waitFor((): void => {
      expect(screen.getByTestId("preset-delete-5")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("preset-delete-5"));
    await waitFor((): void => {
      expect(call).toHaveBeenCalledWith("presets.delete", { id: 5 });
    });
  });

  it("Delete confirm-cancel skips the RPC call", async (): Promise<void> => {
    const { client, call } = makeClient({
      "presets.list": {
        presets: [
          {
            id: 5,
            name: "warmup",
            created_at: "2026-05-17T22:00:00Z",
          },
        ],
      },
    });
    render(
      <PresetPanel
        client={client}
        decks={[emptyDeck("A"), emptyDeck("B")]}
        crossfaderCurve="Linear"
        confirmFn={(): boolean => false}
      />,
    );
    await waitFor((): void => {
      expect(screen.getByTestId("preset-delete-5")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("preset-delete-5"));
    await new Promise((r) => setTimeout(r, 0));
    expect(
      call.mock.calls.some((c) => c[0] === "presets.delete"),
    ).toBe(false);
  });

  it("replayPreset dispatches the expected event count for a fully-populated preset", async (): Promise<void> => {
    const { client, call } = makeClient({});
    // 3 slots × (Assign + 2 Params + WetDry + Enable) = 15
    // + 3 EQ + Pitch + Tempo = 5
    // = 20 per deck × 2 = 40
    // + 1 SetCrossfaderCurve = 41
    const richSlot = {
      effect_id: 2,
      params: { p1: 0.1, p2: 0.2 },
      wet_dry: 0.5,
      enabled: true,
    };
    const richDeck = {
      effects: [richSlot, richSlot, richSlot],
      eq_low_db: 0,
      eq_mid_db: 0,
      eq_high_db: 0,
      pitch_semitones: 0,
      tempo_ratio: 1,
    };
    const rich: Preset = {
      id: 99,
      name: "rich",
      created_at: "",
      deck_a: richDeck,
      deck_b: richDeck,
      crossfader_curve: "Scratch",
    };
    const count = await replayPreset(client, rich);
    expect(count).toBe(41);
    const submitCalls = call.mock.calls.filter(
      (c): boolean => c[0] === "submit_event",
    );
    expect(submitCalls).toHaveLength(41);
  });
});
