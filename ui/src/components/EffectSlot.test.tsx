// EffectSlot.test.tsx — interactive: dropdown change emits EffectAssign /
// EffectClear; wet/dry knob emits EffectWetDry; enabled toggle emits
// EffectEnable; param Knob emits EffectParam; discrete `mode` param
// renders a button-group + emits EffectParam.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { EffectSlot } from "./EffectSlot";
import type { EffectSlotState } from "../store/engine";
import type { EffectManifest } from "../store/effectsManifest";
import type { JsonRpcWS } from "../ws/client";

interface MockBundle {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
}
const makeClient = (): MockBundle => {
  const call = vi.fn().mockResolvedValue(undefined);
  return { call, client: { call } as unknown as JsonRpcWS };
};

const emptySlot = (): EffectSlotState => ({
  effect_id: 0,
  params: {},
  wet_dry: 0.5,
  enabled: false,
});
const filterSlot = (): EffectSlotState => ({
  effect_id: 1,
  params: { cutoff_hz: 1000, resonance: 0.4, mode: 0 },
  wet_dry: 0.7,
  enabled: true,
});

const manifest: EffectManifest = [
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

const submitted = (mb: MockBundle): unknown[] =>
  mb.call.mock.calls
    .filter((args): boolean => args[0] === "submit_event")
    .map((args): unknown => args[1]);

describe("EffectSlot", () => {
  afterEach((): void => {
    cleanup();
  });

  it("dropdown change to non-zero id emits EffectAssign", (): void => {
    const mb = makeClient();
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={emptySlot()}
        manifest={manifest}
        client={mb.client}
      />,
    );
    const select = screen.getByTestId("fx-select-A-0") as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "1" } });
    expect(submitted(mb)).toEqual([
      { EffectAssign: { deck: "A", slot: 0, effect_id: 1 } },
    ]);
  });

  it("dropdown change to 0 emits EffectClear", (): void => {
    const mb = makeClient();
    render(
      <EffectSlot
        deck="B"
        slot={1}
        state={filterSlot()}
        manifest={manifest}
        client={mb.client}
      />,
    );
    const select = screen.getByTestId("fx-select-B-1") as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "0" } });
    expect(submitted(mb)).toEqual([{ EffectClear: { deck: "B", slot: 1 } }]);
  });

  it("wet/dry knob change emits EffectWetDry with parsed value", (): void => {
    const mb = makeClient();
    render(
      <EffectSlot
        deck="A"
        slot={2}
        state={filterSlot()}
        manifest={manifest}
        client={mb.client}
      />,
    );
    const wetdryInput = screen.getByTestId(
      "fx-wetdry-A-2-input",
    ) as HTMLInputElement;
    fireEvent.change(wetdryInput, { target: { value: "0.85" } });
    expect(submitted(mb)).toEqual([
      { EffectWetDry: { deck: "A", slot: 2, value: 0.85 } },
    ]);
  });

  it("enable toggle emits EffectEnable with inverted state", (): void => {
    const mb = makeClient();
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={filterSlot()}
        manifest={manifest}
        client={mb.client}
      />,
    );
    const btn = screen.getByTestId("fx-enable-A-0");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    // filterSlot.enabled = true → toggle should request false
    expect(submitted(mb)).toEqual([
      { EffectEnable: { deck: "A", slot: 0, enabled: false } },
    ]);
  });

  it("enable button is disabled when no effect is assigned", (): void => {
    const mb = makeClient();
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={emptySlot()}
        manifest={manifest}
        client={mb.client}
      />,
    );
    const btn = screen.getByTestId("fx-enable-A-0") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it("continuous param Knob change emits EffectParam", (): void => {
    const mb = makeClient();
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={filterSlot()}
        manifest={manifest}
        client={mb.client}
      />,
    );
    const cutoffInput = screen.getByTestId(
      "fx-A-0-param-cutoff_hz-input",
    ) as HTMLInputElement;
    fireEvent.change(cutoffInput, { target: { value: "2500" } });
    expect(submitted(mb)).toEqual([
      {
        EffectParam: { deck: "A", slot: 0, param: "cutoff_hz", value: 2500 },
      },
    ]);
  });

  it("discrete `mode` param renders a button-group and emits EffectParam on click", (): void => {
    const mb = makeClient();
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={filterSlot()}
        manifest={manifest}
        client={mb.client}
      />,
    );
    const hpBtn = screen.getByTestId("fx-A-0-mode-1");
    fireEvent.pointerDown(hpBtn);
    fireEvent.pointerUp(hpBtn);
    expect(submitted(mb)).toEqual([
      { EffectParam: { deck: "A", slot: 0, param: "mode", value: 1 } },
    ]);
  });

  it("renders no param controls for empty slot (effect_id=0)", (): void => {
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={emptySlot()}
        manifest={manifest}
        client={makeClient().client}
      />,
    );
    expect(screen.queryByTestId("fx-params-A-0")).toBeNull();
  });

  it("renders no one-shot row for empty slot", (): void => {
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={emptySlot()}
        manifest={manifest}
        client={makeClient().client}
      />,
    );
    expect(screen.queryByTestId("fx-oneshot-row-A-0")).toBeNull();
  });

  it("renders 4 one-shot preset buttons on an assigned slot", (): void => {
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={filterSlot()}
        manifest={manifest}
        client={makeClient().client}
      />,
    );
    expect(screen.getByTestId("fx-oneshot-A-0-1")).toBeTruthy();
    expect(screen.getByTestId("fx-oneshot-A-0-4")).toBeTruthy();
    expect(screen.getByTestId("fx-oneshot-A-0-8")).toBeTruthy();
    expect(screen.getByTestId("fx-oneshot-A-0-16")).toBeTruthy();
  });

  it("clicking a one-shot preset emits EffectOneShot with the beats payload", (): void => {
    const mb = makeClient();
    render(
      <EffectSlot
        deck="B"
        slot={2}
        state={filterSlot()}
        manifest={manifest}
        client={mb.client}
      />,
    );
    const btn = screen.getByTestId("fx-oneshot-B-2-4");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(submitted(mb)).toContainEqual({
      EffectOneShot: { deck: "B", slot: 2, beats: 4 },
    });
  });

  it("renders countdown when one_shot is in flight", (): void => {
    const futureMicros = Date.now() * 1000 + 2_000_000; // ~2 s remaining
    const state: EffectSlotState = {
      ...filterSlot(),
      one_shot: { ends_at_micros: futureMicros, was_enabled: false },
    };
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={state}
        manifest={manifest}
        client={makeClient().client}
      />,
    );
    const countdown = screen.getByTestId("fx-oneshot-countdown-A-0");
    expect(countdown.textContent).toMatch(/\d+\s*ms/);
  });

  it("no countdown when one_shot has elapsed", (): void => {
    const state: EffectSlotState = {
      ...filterSlot(),
      one_shot: {
        ends_at_micros: Date.now() * 1000 - 1_000_000,
        was_enabled: false,
      },
    };
    render(
      <EffectSlot
        deck="A"
        slot={0}
        state={state}
        manifest={manifest}
        client={makeClient().client}
      />,
    );
    expect(screen.queryByTestId("fx-oneshot-countdown-A-0")).toBeNull();
  });
});
