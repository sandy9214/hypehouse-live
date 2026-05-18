// EffectRack.test.tsx — renders 3 slot cards and threads props through.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { EffectRack } from "./EffectRack";
import type { EffectSlotState } from "../store/engine";
import type { EffectManifest } from "../store/effectsManifest";
import type { JsonRpcWS } from "../ws/client";

const makeClient = (): JsonRpcWS =>
  ({ call: vi.fn().mockResolvedValue(undefined) }) as unknown as JsonRpcWS;

const emptySlot = (): EffectSlotState => ({
  effect_id: 0,
  params: {},
  wet_dry: 0.5,
  enabled: false,
});

const manifestFixture: EffectManifest = [
  {
    id: 1,
    name: "filter",
    params: [
      { name: "cutoff_hz", min: 20, max: 20000, default: 500 },
      { name: "resonance", min: 0, max: 1, default: 0.3 },
      { name: "mode", min: 0, max: 2, default: 0 },
    ],
  },
  {
    id: 2,
    name: "echo",
    params: [
      { name: "time_ms", min: 10, max: 2000, default: 250 },
      { name: "feedback", min: 0, max: 0.95, default: 0.45 },
      { name: "tone", min: -1, max: 1, default: 0 },
    ],
  },
];

describe("EffectRack", () => {
  afterEach((): void => {
    cleanup();
  });

  it("renders exactly 3 slot cards per deck", (): void => {
    render(
      <EffectRack
        deck="A"
        effects={[emptySlot(), emptySlot(), emptySlot()]}
        manifest={manifestFixture}
        client={makeClient()}
      />,
    );
    expect(screen.getByTestId("fx-slot-A-0")).toBeTruthy();
    expect(screen.getByTestId("fx-slot-A-1")).toBeTruthy();
    expect(screen.getByTestId("fx-slot-A-2")).toBeTruthy();
  });

  it("scopes slot test-ids per deck so two decks don't collide", (): void => {
    render(
      <div>
        <EffectRack
          deck="A"
          effects={[emptySlot(), emptySlot(), emptySlot()]}
          manifest={manifestFixture}
          client={makeClient()}
        />
        <EffectRack
          deck="B"
          effects={[emptySlot(), emptySlot(), emptySlot()]}
          manifest={manifestFixture}
          client={makeClient()}
        />
      </div>,
    );
    expect(screen.getByTestId("fx-slot-A-0")).toBeTruthy();
    expect(screen.getByTestId("fx-slot-B-0")).toBeTruthy();
    expect(screen.getByTestId("fx-rack-A")).toBeTruthy();
    expect(screen.getByTestId("fx-rack-B")).toBeTruthy();
  });
});
