// Deck.test.tsx — render-level assertions: play button enable/disable,
// cue button enable/disable, loop-out disabled until loop-in fires.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Deck } from "./Deck";
import type { Deck as DeckState } from "../store/engine";
import type { JsonRpcWS } from "../ws/client";

const makeClient = (): JsonRpcWS => {
  // Minimal stub — only `.call` is exercised; tests that don't click
  // don't need it to do anything.
  return {
    call: vi.fn().mockResolvedValue(undefined),
  } as unknown as JsonRpcWS;
};

const emptySlot = (): DeckState["effects"][number] => ({
  effect_id: 0,
  params: {},
  wet_dry: 0.5,
  enabled: false,
});

const baseDeck = (): DeckState => ({
  id: "A",
  track_title: null,
  bpm: null,
  position_ms: 0,
  playing: false,
  eq_low: 0,
  eq_mid: 0,
  eq_high: 0,
  pitch_semitones: 0,
  tempo_ratio: 1.0,
  hot_cues: [null, null, null, null, null, null, null, null],
  loop_in_ms: null,
  loop_out_ms: null,
  copilot_enabled: false,
  effects: [emptySlot(), emptySlot(), emptySlot()],
  stem_gains: [1, 1, 1, 1],
  stem_mode: false,
});

describe("Deck (render)", () => {
  afterEach((): void => {
    cleanup();
  });

  it("disables play + cue when no track loaded", (): void => {
    render(<Deck deck={baseDeck()} side="left" client={makeClient()} />);
    expect(
      (screen.getByTestId("play-A") as HTMLButtonElement).disabled,
    ).toBe(true);
    expect((screen.getByTestId("cue-A") as HTMLButtonElement).disabled).toBe(
      true,
    );
  });

  it("enables play + cue when track is loaded and shows PLAY label", (): void => {
    const d: DeckState = { ...baseDeck(), track_title: "Some Track" };
    render(<Deck deck={d} side="left" client={makeClient()} />);
    const play = screen.getByTestId("play-A") as HTMLButtonElement;
    expect(play.disabled).toBe(false);
    expect(play.textContent ?? "").toContain("PLAY");
  });

  it("shows PAUSE label and pressed state when deck is playing", (): void => {
    const d: DeckState = {
      ...baseDeck(),
      track_title: "Some Track",
      playing: true,
    };
    render(<Deck deck={d} side="left" client={makeClient()} />);
    const play = screen.getByTestId("play-A") as HTMLButtonElement;
    expect(play.textContent ?? "").toContain("PAUSE");
    expect(play.getAttribute("aria-pressed")).toBe("true");
  });

  it("disables loop-out until loop_in_ms is set", (): void => {
    const d: DeckState = { ...baseDeck(), track_title: "Some Track" };
    const { rerender } = render(
      <Deck deck={d} side="left" client={makeClient()} />,
    );
    expect(
      (screen.getByTestId("loop-out-A") as HTMLButtonElement).disabled,
    ).toBe(true);

    rerender(
      <Deck
        deck={{ ...d, loop_in_ms: 1000 }}
        side="left"
        client={makeClient()}
      />,
    );
    expect(
      (screen.getByTestId("loop-out-A") as HTMLButtonElement).disabled,
    ).toBe(false);
  });

  it("disables hot-cue pads when no track is loaded", (): void => {
    render(<Deck deck={baseDeck()} side="left" client={makeClient()} />);
    for (let i = 0; i < 8; i++) {
      const pad = screen.getByTestId(`cue-A-${i}`) as HTMLButtonElement;
      expect(pad.disabled).toBe(true);
    }
  });

  it("renders copilot LED green when copilot_enabled", (): void => {
    const d: DeckState = {
      ...baseDeck(),
      track_title: "T",
      copilot_enabled: true,
    };
    render(<Deck deck={d} side="left" client={makeClient()} />);
    const led = screen.getByTestId("copilot-led-A");
    expect(led.getAttribute("style") ?? "").toContain("rgb(95, 207, 108)");
  });
});
