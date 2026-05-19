// SidechainPanel.test.tsx — toggle + trigger switch + knob events.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SidechainPanel } from "./SidechainPanel";
import type { JsonRpcWS } from "../ws/client";
import type { SidechainConfig } from "../store/engine";

interface MockBundle {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
}
const makeClient = (): MockBundle => {
  const call = vi.fn().mockResolvedValue(undefined);
  return { call, client: { call } as unknown as JsonRpcWS };
};

const submitted = (mb: MockBundle): unknown[] =>
  mb.call.mock.calls
    .filter((args): boolean => args[0] === "submit_event")
    .map((args): unknown => args[1]);

const defaultState: SidechainConfig = {
  enabled: false,
  trigger_deck: "A",
  threshold_db: -12,
  ratio: 4,
  attack_ms: 5,
  release_ms: 200,
  makeup_gain_db: 0,
};

describe("SidechainPanel", () => {
  afterEach((): void => {
    cleanup();
  });

  it("renders with default state (off, trigger A)", () => {
    render(<SidechainPanel client={makeClient().client} state={defaultState} />);
    const toggle = screen.getByTestId("sidechain-toggle");
    expect(toggle.textContent).toBe("OFF");
    expect(toggle.getAttribute("aria-pressed")).toBe("false");
    expect(
      screen.getByTestId("sidechain-trigger-A").getAttribute("aria-pressed"),
    ).toBe("true");
    expect(
      screen.getByTestId("sidechain-trigger-B").getAttribute("aria-pressed"),
    ).toBe("false");
  });

  it("falls back to defaults when state is null", () => {
    render(<SidechainPanel client={makeClient().client} state={null} />);
    expect(screen.getByTestId("sidechain-toggle").textContent).toBe("OFF");
  });

  it("toggle button emits SetSidechainEnabled with inverted state", () => {
    const mb = makeClient();
    render(<SidechainPanel client={mb.client} state={defaultState} />);
    const btn = screen.getByTestId("sidechain-toggle");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(submitted(mb)).toContainEqual({
      SetSidechainEnabled: { enabled: true },
    });
  });

  it("clicking trigger B emits SetSidechainParams with deck B", () => {
    const mb = makeClient();
    render(<SidechainPanel client={mb.client} state={defaultState} />);
    const btn = screen.getByTestId("sidechain-trigger-B");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(submitted(mb)).toContainEqual({
      SetSidechainParams: {
        trigger_deck: "B",
        threshold_db: null,
        ratio: null,
        attack_ms: null,
        release_ms: null,
        makeup_gain_db: null,
      },
    });
  });

  it("renders all 5 param knobs", () => {
    render(<SidechainPanel client={makeClient().client} state={defaultState} />);
    expect(screen.getByTestId("sidechain-threshold")).toBeTruthy();
    expect(screen.getByTestId("sidechain-ratio")).toBeTruthy();
    expect(screen.getByTestId("sidechain-attack")).toBeTruthy();
    expect(screen.getByTestId("sidechain-release")).toBeTruthy();
    expect(screen.getByTestId("sidechain-makeup")).toBeTruthy();
  });

  it("renders enabled state from props", () => {
    const onState: SidechainConfig = { ...defaultState, enabled: true };
    render(<SidechainPanel client={makeClient().client} state={onState} />);
    expect(screen.getByTestId("sidechain-toggle").textContent).toBe("ON");
    expect(
      screen.getByTestId("sidechain-toggle").getAttribute("aria-pressed"),
    ).toBe("true");
  });

  it("renders trigger=B state from props", () => {
    const bState: SidechainConfig = { ...defaultState, trigger_deck: "B" };
    render(<SidechainPanel client={makeClient().client} state={bState} />);
    expect(
      screen.getByTestId("sidechain-trigger-A").getAttribute("aria-pressed"),
    ).toBe("false");
    expect(
      screen.getByTestId("sidechain-trigger-B").getAttribute("aria-pressed"),
    ).toBe("true");
  });

  it("GR meter empty when grDb is 0 or undefined", () => {
    const { rerender } = render(
      <SidechainPanel client={makeClient().client} state={defaultState} />,
    );
    const fill = screen.getByTestId("sidechain-gr-meter-fill") as HTMLDivElement;
    expect(fill.style.height).toBe("0px");
    rerender(
      <SidechainPanel client={makeClient().client} state={defaultState} grDb={0} />,
    );
    expect(fill.style.height).toBe("0px");
  });

  it("GR meter fills proportional to dB reduction", () => {
    render(
      <SidechainPanel
        client={makeClient().client}
        state={defaultState}
        grDb={-12}
      />,
    );
    const fill = screen.getByTestId("sidechain-gr-meter-fill") as HTMLDivElement;
    // METER_HEIGHT=60, METER_MIN_DB=-24 → -12 is half → 30 px
    expect(fill.style.height).toBe("30px");
  });

  it("GR meter clamps reduction past -24 dB to full", () => {
    render(
      <SidechainPanel
        client={makeClient().client}
        state={defaultState}
        grDb={-48}
      />,
    );
    const fill = screen.getByTestId("sidechain-gr-meter-fill") as HTMLDivElement;
    expect(fill.style.height).toBe("60px");
  });

  it("GR meter ignores positive / non-finite values", () => {
    render(
      <SidechainPanel
        client={makeClient().client}
        state={defaultState}
        grDb={Number.NaN}
      />,
    );
    const fill = screen.getByTestId("sidechain-gr-meter-fill") as HTMLDivElement;
    expect(fill.style.height).toBe("0px");
  });
});
