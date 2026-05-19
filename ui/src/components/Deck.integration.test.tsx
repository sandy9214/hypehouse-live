// Deck.integration.test.tsx — interactive end-to-end: click controls
// and assert the matching `submit_event` payload is dispatched.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Deck } from "./Deck";
import type { Deck as DeckState } from "../store/engine";
import type { JsonRpcWS } from "../ws/client";
import { __resetEffectsManifest } from "../store/effectsManifest";
import {
  __resetHotCuePersist,
  noteLoadedTrack,
} from "../store/hotCuePersist";

interface MockBundle {
  client: JsonRpcWS;
  call: ReturnType<typeof vi.fn>;
}

const makeClient = (): MockBundle => {
  const call = vi.fn().mockResolvedValue(undefined);
  const client = { call } as unknown as JsonRpcWS;
  return { client, call };
};

const emptySlot = (): DeckState["effects"][number] => ({
  effect_id: 0,
  params: {},
  wet_dry: 0.5,
  enabled: false,
});

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
  tempo_ratio: 1.0,
  hot_cues: [null, null, null, null, null, null, null, null],
  loop_in_ms: null,
  loop_out_ms: null,
  loop_active: false,
  beat_period_ms: 0,
  copilot_enabled: false,
  effects: [emptySlot(), emptySlot(), emptySlot()],
  stem_gains: [1, 1, 1, 1],
  stem_mode: false,
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
    __resetEffectsManifest();
    __resetHotCuePersist();
  });
  afterEach((): void => {
    vi.useRealTimers();
    cleanup();
    __resetEffectsManifest();
    __resetHotCuePersist();
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

  it("emits TempoBend with ratio = 1 + pct/100 when tempo knob changes", (): void => {
    // Pioneer-style tempo slider — UI works in percent, wire payload
    // carries the engine's `tempo_ratio` so the reducer math (clamp +
    // reducer side effects) is unambiguous.
    const mb = makeClient();
    render(<Deck deck={loadedDeck()} side="left" client={mb.client} />);
    const input = screen.getByTestId("tempo-A-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "3" } });
    const events = submittedEvents(mb);
    expect(events).toHaveLength(1);
    // floating-point equality: tolerate ULP drift.
    const evt = events[0] as { TempoBend: { deck: string; ratio: number } };
    expect(evt.TempoBend.deck).toBe("A");
    expect(evt.TempoBend.ratio).toBeCloseTo(1.03, 6);
  });

  it("negative tempo percent emits ratio < 1", (): void => {
    const mb = makeClient();
    render(<Deck deck={loadedDeck()} side="left" client={mb.client} />);
    const input = screen.getByTestId("tempo-A-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "-5" } });
    const evt = submittedEvents(mb)[0] as {
      TempoBend: { deck: string; ratio: number };
    };
    expect(evt.TempoBend.ratio).toBeCloseTo(0.95, 6);
  });

  it("renders effective BPM (= bpm × tempo_ratio) with ± delta marker", (): void => {
    // Operator-visible feedback: when tempo_ratio drifts away from 1.0
    // the BPM cell shows the *effective* BPM plus a tiny "+<delta>" or
    // "-<delta>" inline marker. The marker disappears when the ratio
    // is exactly 1.0 (no drift to communicate).
    const mb = makeClient();
    render(
      <Deck
        deck={loadedDeck({ bpm: 120, tempo_ratio: 1.04 })}
        side="left"
        client={mb.client}
      />,
    );
    const cell = screen.getByTestId("bpm-A");
    // Effective BPM = 120 × 1.04 = 124.80
    expect(cell.textContent).toContain("124.80");
    const delta = screen.getByTestId("bpm-delta-A");
    expect(delta.textContent).toBe("+4.80");
  });

  it("hides BPM delta marker when tempo_ratio is exactly 1.0", (): void => {
    const mb = makeClient();
    render(
      <Deck
        deck={loadedDeck({ bpm: 128 })}
        side="left"
        client={mb.client}
      />,
    );
    const cell = screen.getByTestId("bpm-A");
    expect(cell.textContent).toContain("128.00");
    expect(screen.queryByTestId("bpm-delta-A")).toBeNull();
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

  it("HotCueSet (long-press) triggers library.set_hot_cues after 500ms debounce", (): void => {
    const mb = makeClient();
    // Pretend a library track is loaded on deck A — Deck.tsx's drop
    // handler / TrackRow click normally registers this binding.
    noteLoadedTrack("A", "song-7");
    render(
      <Deck
        deck={loadedDeck({ position_ms: 7777 })}
        side="left"
        client={mb.client}
      />,
    );
    const pad = screen.getByTestId("cue-A-2");

    // Long press → HotCueSet (engine-side) + queued library write.
    fireEvent.pointerDown(pad);
    vi.advanceTimersByTime(500);
    fireEvent.pointerUp(pad);

    // Engine event fired immediately.
    const engineEvents = submittedEvents(mb);
    expect(engineEvents).toEqual([
      { HotCueSet: { deck: "A", slot: 2, position_ms: 7777 } },
    ]);

    // Library write hasn't fired yet — debounce window is 500ms.
    const libCallsBefore = mb.call.mock.calls.filter(
      (c): boolean => c[0] === "library.set_hot_cues",
    );
    expect(libCallsBefore).toHaveLength(0);

    // After the debounce window, the library write fires once.
    vi.advanceTimersByTime(550);
    const libCalls = mb.call.mock.calls.filter(
      (c): boolean => c[0] === "library.set_hot_cues",
    );
    expect(libCalls).toHaveLength(1);
    const params = libCalls[0][1] as {
      track_id: string;
      hot_cues: ReadonlyArray<number | null>;
    };
    expect(params.track_id).toBe("song-7");
    // Slot 2 set to current position; other slots untouched (null).
    expect(params.hot_cues[2]).toBe(7777);
    expect(params.hot_cues.filter((v): boolean => v !== null)).toEqual([7777]);
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
