// Deck.integration.test.tsx — interactive end-to-end: click controls
// and assert the matching `submit_event` payload is dispatched.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Deck } from "./Deck";
import type { Deck as DeckState } from "../store/engine";
import type { JsonRpcWS } from "../ws/client";

interface MockBundle {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
}

const makeClient = (): MockBundle => {
  const call = vi.fn().mockResolvedValue(undefined);
  const client = { call } as unknown as JsonRpcWS;
  return { client, call };
};

const loadedDeck = (overrides: Partial<DeckState> = {}): DeckState => ({
  id: "A",
  track_title: "Loaded Track",
  bpm: 120,
  position_ms: 4321,
  playing: false,
  eq_low: 0,
  eq_mid: 0,
  eq_high: 0,
  pitch_semitones: 0,
  hot_cues: [null, null, null, null, null, null, null, null],
  loop_in_ms: null,
  loop_out_ms: null,
  copilot_enabled: false,
  ...overrides,
});

// Returns just the EventKind payloads pushed via submit_event.
const submittedEvents = (mb: MockBundle): unknown[] =>
  mb.call.mock.calls
    .filter((args): boolean => args[0] === "submit_event")
    .map((args): unknown => args[1]);

describe("Deck (integration)", () => {
  beforeEach((): void => {
    vi.useFakeTimers();
  });
  afterEach((): void => {
    vi.useRealTimers();
    cleanup();
  });

  it("emits DeckPlay when play button is clicked on a paused loaded deck", (): void => {
    const mb = makeClient();
    render(<Deck deck={loadedDeck()} side="left" client={mb.client} />);
    const btn = screen.getByTestId("play-A");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(submittedEvents(mb)).toEqual([{ DeckPlay: { deck: "A" } }]);
  });

  it("emits DeckPause when play button is clicked while playing", (): void => {
    const mb = makeClient();
    render(
      <Deck
        deck={loadedDeck({ playing: true })}
        side="left"
        client={mb.client}
      />,
    );
    const btn = screen.getByTestId("play-A");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(submittedEvents(mb)).toEqual([{ DeckPause: { deck: "A" } }]);
  });

  it("emits DeckCue with current position_ms when cue button clicked", (): void => {
    const mb = makeClient();
    render(
      <Deck
        deck={loadedDeck({ position_ms: 9999 })}
        side="left"
        client={mb.client}
      />,
    );
    const btn = screen.getByTestId("cue-A");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);
    expect(submittedEvents(mb)).toEqual([
      { DeckCue: { deck: "A", position_ms: 9999 } },
    ]);
  });

  it("emits LoopIn / LoopOut and respects loop-out disabled state", (): void => {
    const mb = makeClient();
    const { rerender } = render(
      <Deck deck={loadedDeck()} side="left" client={mb.client} />,
    );
    const loopIn = screen.getByTestId("loop-in-A");
    fireEvent.pointerDown(loopIn);
    fireEvent.pointerUp(loopIn);

    rerender(
      <Deck
        deck={loadedDeck({ loop_in_ms: 1000 })}
        side="left"
        client={mb.client}
      />,
    );
    const loopOut = screen.getByTestId("loop-out-A");
    fireEvent.pointerDown(loopOut);
    fireEvent.pointerUp(loopOut);

    expect(submittedEvents(mb)).toEqual([
      { LoopIn: { deck: "A" } },
      { LoopOut: { deck: "A" } },
    ]);
  });

  it("emits EqAdjust with value_db when an EQ knob changes", (): void => {
    const mb = makeClient();
    render(<Deck deck={loadedDeck()} side="left" client={mb.client} />);
    const input = screen.getByTestId("eq-mid-A-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "3" } });
    expect(submittedEvents(mb)).toEqual([
      { EqAdjust: { deck: "A", band: "Mid", value_db: 3 } },
    ]);
  });

  it("emits PitchBend (semitones) when pitch knob changes", (): void => {
    const mb = makeClient();
    render(<Deck deck={loadedDeck()} side="left" client={mb.client} />);
    const input = screen.getByTestId("pitch-A-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "-4.2" } });
    expect(submittedEvents(mb)).toEqual([
      { PitchBend: { deck: "A", semitones: -4.2 } },
    ]);
  });

  it("emits HotCueTrigger on short-press, HotCueSet on long-press", (): void => {
    const mb = makeClient();
    render(
      <Deck
        deck={loadedDeck({ position_ms: 7777 })}
        side="left"
        client={mb.client}
      />,
    );
    const pad = screen.getByTestId("cue-A-2");

    // Short press → trigger
    fireEvent.pointerDown(pad);
    fireEvent.pointerUp(pad);

    // Long press → set @ current position
    fireEvent.pointerDown(pad);
    vi.advanceTimersByTime(500);
    fireEvent.pointerUp(pad);

    expect(submittedEvents(mb)).toEqual([
      { HotCueTrigger: { deck: "A", slot: 2 } },
      { HotCueSet: { deck: "A", slot: 2, position_ms: 7777 } },
    ]);
  });

  it("emits CopilotEngage when off and CopilotDisengage when on", (): void => {
    const mb = makeClient();
    const { rerender } = render(
      <Deck deck={loadedDeck()} side="left" client={mb.client} />,
    );
    const btn = screen.getByTestId("copilot-A");
    fireEvent.pointerDown(btn);
    fireEvent.pointerUp(btn);

    rerender(
      <Deck
        deck={loadedDeck({ copilot_enabled: true })}
        side="left"
        client={mb.client}
      />,
    );
    const btn2 = screen.getByTestId("copilot-A");
    fireEvent.pointerDown(btn2);
    fireEvent.pointerUp(btn2);

    expect(submittedEvents(mb)).toEqual([
      { CopilotEngage: { deck: "A" } },
      { CopilotDisengage: { deck: "A" } },
    ]);
  });
});
