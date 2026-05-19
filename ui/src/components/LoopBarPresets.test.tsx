// LoopBarPresets.test.tsx — render + interaction assertions for the
// bar-aware auto-loop pad row. Mirrors the engine-side test coverage in
// `engine/src/state.rs` (`set_loop_bars_*`) so the wire contract holds
// end-to-end.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import {
  activeLoopBars,
  LOOP_BAR_PRESETS,
  LoopBarPresets,
} from "./LoopBarPresets";
import type { Deck as DeckState } from "../store/engine";
import type { JsonRpcWS } from "../ws/client";

const emptySlot = (): DeckState["effects"][number] => ({
  effect_id: 0,
  params: {},
  wet_dry: 0.5,
  enabled: false,
});

const makeDeck = (over: Partial<DeckState> = {}): DeckState => ({
  id: "A",
  track_title: "Demo",
  bpm: 120,
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
  loop_active: false,
  beat_period_ms: 500, // 120 BPM
  copilot_enabled: false,
  effects: [emptySlot(), emptySlot(), emptySlot()],
  stem_gains: [1, 1, 1, 1],
  stem_mode: false,
  ...over,
});

const makeClient = (): { client: JsonRpcWS; call: ReturnType<typeof vi.fn> } => {
  const call = vi.fn().mockResolvedValue(undefined);
  return { client: { call } as unknown as JsonRpcWS, call };
};

describe("LoopBarPresets", () => {
  afterEach((): void => {
    cleanup();
  });

  it("renders 5 pad buttons labelled 1 / 2 / 4 / 8 / 16", (): void => {
    render(<LoopBarPresets deck={makeDeck()} client={makeClient().client} />);
    for (const bars of LOOP_BAR_PRESETS) {
      const btn = screen.getByTestId(`loop-bars-A-${bars}`);
      expect(btn.textContent).toBe(String(bars));
    }
  });

  it("emits SetLoopBars with the clicked preset", (): void => {
    const { client, call } = makeClient();
    render(<LoopBarPresets deck={makeDeck()} client={client} />);
    fireEvent.click(screen.getByTestId("loop-bars-A-4"));
    expect(call).toHaveBeenCalledWith("submit_event", {
      SetLoopBars: { deck: "A", bars: 4 },
    });
  });

  it("disables every preset when the deck has no beat grid", (): void => {
    const { client, call } = makeClient();
    render(
      <LoopBarPresets
        deck={makeDeck({ beat_period_ms: 0 })}
        client={client}
      />,
    );
    for (const bars of LOOP_BAR_PRESETS) {
      const btn = screen.getByTestId(`loop-bars-A-${bars}`) as HTMLButtonElement;
      expect(btn.disabled).toBe(true);
    }
    fireEvent.click(screen.getByTestId("loop-bars-A-4"));
    expect(call).not.toHaveBeenCalled();
  });

  it("highlights the active preset when the current loop length matches", (): void => {
    // 4-bar loop @ 120 BPM = 8000 ms — should mark the "4" button pressed.
    const deck = makeDeck({
      loop_active: true,
      loop_in_ms: 0,
      loop_out_ms: 8000,
    });
    render(<LoopBarPresets deck={deck} client={makeClient().client} />);
    expect(
      screen.getByTestId("loop-bars-A-4").getAttribute("aria-pressed"),
    ).toBe("true");
    expect(
      screen.getByTestId("loop-bars-A-1").getAttribute("aria-pressed"),
    ).toBe("false");
  });

  it("does not highlight any preset when no loop is armed", (): void => {
    render(<LoopBarPresets deck={makeDeck()} client={makeClient().client} />);
    for (const bars of LOOP_BAR_PRESETS) {
      expect(
        screen.getByTestId(`loop-bars-A-${bars}`).getAttribute("aria-pressed"),
      ).toBe("false");
    }
  });

  it("activeLoopBars matches the engine snap math", (): void => {
    // 1-bar loop @ 120 BPM.
    expect(
      activeLoopBars(
        makeDeck({
          loop_active: true,
          loop_in_ms: 0,
          loop_out_ms: 2000,
        }),
      ),
    ).toBe(1);
    // 8-bar loop @ 128 BPM (beat_period ≈ 468.75 ms; bar ≈ 1875 ms).
    expect(
      activeLoopBars(
        makeDeck({
          loop_active: true,
          loop_in_ms: 0,
          loop_out_ms: 15_000,
          beat_period_ms: 468.75,
        }),
      ),
    ).toBe(8);
    // Unarmed loop → null.
    expect(activeLoopBars(makeDeck())).toBeNull();
    // Manual loop that doesn't match any preset (e.g. 1.3 bars) → null.
    expect(
      activeLoopBars(
        makeDeck({
          loop_active: true,
          loop_in_ms: 0,
          loop_out_ms: 2600,
        }),
      ),
    ).toBeNull();
  });
});
