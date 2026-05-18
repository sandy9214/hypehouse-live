// EffectRack.test.tsx — renders 3 slot cards + threads props through,
// + exercises the drag-drop and keyboard reorder UX added in the
// effects-reorder PR. Every reorder gesture lands as a single
// `submit_event` call with an `EffectSwapSlots` payload.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { EffectRack } from "./EffectRack";
import type { EffectSlotState } from "../store/engine";
import type { EffectManifest } from "../store/effectsManifest";
import type { JsonRpcWS } from "../ws/client";

/** Mini DataTransfer shim — jsdom's drag events come with `dataTransfer=null`,
 * so we plug in just the surface our component reads. */
interface DTLike {
  data: Map<string, string>;
  types: string[];
  effectAllowed: string;
  dropEffect: string;
  setData: (k: string, v: string) => void;
  getData: (k: string) => string;
}
const makeDataTransfer = (): DTLike => {
  const data = new Map<string, string>();
  const types: string[] = [];
  return {
    data,
    types,
    effectAllowed: "none",
    dropEffect: "none",
    setData(k, v): void {
      data.set(k, v);
      if (!types.includes(k)) types.push(k);
    },
    getData(k): string {
      return data.get(k) ?? "";
    },
  };
};

interface MockClient {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
}
const makeClient = (): MockClient => {
  const call = vi.fn().mockResolvedValue(undefined);
  return { call, client: { call } as unknown as JsonRpcWS };
};

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

const renderRack = (client: JsonRpcWS): void => {
  render(
    <EffectRack
      deck="A"
      effects={[emptySlot(), emptySlot(), emptySlot()]}
      manifest={manifestFixture}
      client={client}
    />,
  );
};

/** Drive a full drag from source-handle → target-handle via fireEvent
 * with a shared DataTransfer. */
const dragSlot = (sourceIdx: number, targetIdx: number): void => {
  const dt = makeDataTransfer();
  const src = screen.getByTestId(`fx-slot-handle-A-${sourceIdx}`);
  const tgt = screen.getByTestId(`fx-slot-handle-A-${targetIdx}`);
  fireEvent.dragStart(src, { dataTransfer: dt });
  fireEvent.dragOver(tgt, { dataTransfer: dt });
  fireEvent.drop(tgt, { dataTransfer: dt });
  fireEvent.dragEnd(src, { dataTransfer: dt });
};

describe("EffectRack", () => {
  afterEach((): void => {
    cleanup();
  });

  it("renders exactly 3 slot cards per deck", (): void => {
    const { client } = makeClient();
    render(
      <EffectRack
        deck="A"
        effects={[emptySlot(), emptySlot(), emptySlot()]}
        manifest={manifestFixture}
        client={client}
      />,
    );
    expect(screen.getByTestId("fx-slot-A-0")).toBeTruthy();
    expect(screen.getByTestId("fx-slot-A-1")).toBeTruthy();
    expect(screen.getByTestId("fx-slot-A-2")).toBeTruthy();
  });

  it("scopes slot test-ids per deck so two decks don't collide", (): void => {
    const { client: c1 } = makeClient();
    const { client: c2 } = makeClient();
    render(
      <div>
        <EffectRack
          deck="A"
          effects={[emptySlot(), emptySlot(), emptySlot()]}
          manifest={manifestFixture}
          client={c1}
        />
        <EffectRack
          deck="B"
          effects={[emptySlot(), emptySlot(), emptySlot()]}
          manifest={manifestFixture}
          client={c2}
        />
      </div>,
    );
    expect(screen.getByTestId("fx-slot-A-0")).toBeTruthy();
    expect(screen.getByTestId("fx-slot-B-0")).toBeTruthy();
    expect(screen.getByTestId("fx-rack-A")).toBeTruthy();
    expect(screen.getByTestId("fx-rack-B")).toBeTruthy();
  });

  it("drag slot 0 → slot 2 emits EffectSwapSlots(0, 2)", (): void => {
    const { call, client } = makeClient();
    renderRack(client);
    dragSlot(0, 2);
    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith("submit_event", {
      EffectSwapSlots: { deck: "A", slot_a: 0, slot_b: 2 },
    });
  });

  it("drag slot 2 → slot 0 emits EffectSwapSlots(2, 0)", (): void => {
    // Reverse direction — verifies (a, b) match the source → target,
    // not always (lower, higher).
    const { call, client } = makeClient();
    renderRack(client);
    dragSlot(2, 0);
    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith("submit_event", {
      EffectSwapSlots: { deck: "A", slot_a: 2, slot_b: 0 },
    });
  });

  it("drop onto the same slot is a no-op (no submit_event)", (): void => {
    const { call, client } = makeClient();
    renderRack(client);
    dragSlot(1, 1);
    expect(call).not.toHaveBeenCalled();
  });

  it("Shift+ArrowUp on slot 1 emits EffectSwapSlots(1, 0)", (): void => {
    const { call, client } = makeClient();
    renderRack(client);
    const slot1 = screen.getByTestId("fx-slot-handle-A-1");
    fireEvent.keyDown(slot1, { key: "ArrowUp", shiftKey: true });
    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith("submit_event", {
      EffectSwapSlots: { deck: "A", slot_a: 1, slot_b: 0 },
    });
  });

  it("Shift+ArrowDown on slot 2 (last slot) is a no-op", (): void => {
    const { call, client } = makeClient();
    renderRack(client);
    const slot2 = screen.getByTestId("fx-slot-handle-A-2");
    fireEvent.keyDown(slot2, { key: "ArrowDown", shiftKey: true });
    expect(call).not.toHaveBeenCalled();
  });

  it("Shift+ArrowUp on slot 0 (first slot) is a no-op", (): void => {
    // Symmetric edge guard for the top of the chain.
    const { call, client } = makeClient();
    renderRack(client);
    const slot0 = screen.getByTestId("fx-slot-handle-A-0");
    fireEvent.keyDown(slot0, { key: "ArrowUp", shiftKey: true });
    expect(call).not.toHaveBeenCalled();
  });

  it("ArrowUp without Shift is ignored (no submit_event)", (): void => {
    // The shift modifier is mandatory so plain Arrow keys can still
    // drive other UI (e.g. <select>) on a focused slot.
    const { call, client } = makeClient();
    renderRack(client);
    const slot1 = screen.getByTestId("fx-slot-handle-A-1");
    fireEvent.keyDown(slot1, { key: "ArrowUp", shiftKey: false });
    expect(call).not.toHaveBeenCalled();
  });

  it("marks source slot during drag and clears on drop", (): void => {
    // Visual-feedback contract: dragSource attribute appears on the
    // grabbed slot for the duration of the drag.
    const { client } = makeClient();
    renderRack(client);
    const dt = makeDataTransfer();
    const src = screen.getByTestId("fx-slot-handle-A-0");
    const tgt = screen.getByTestId("fx-slot-handle-A-2");
    fireEvent.dragStart(src, { dataTransfer: dt });
    expect(src.getAttribute("data-drag-source")).toBe("true");
    fireEvent.dragOver(tgt, { dataTransfer: dt });
    expect(tgt.getAttribute("data-drop-target")).toBe("true");
    fireEvent.drop(tgt, { dataTransfer: dt });
    expect(src.getAttribute("data-drag-source")).toBeNull();
    expect(tgt.getAttribute("data-drop-target")).toBeNull();
  });
});
