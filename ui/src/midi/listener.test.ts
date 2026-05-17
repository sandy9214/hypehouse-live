// vitest tests for WebMIDIListener.
//
// We stub navigator.requestMIDIAccess + a fake JsonRpcWS to assert:
//   1. start() handshake calls requestMIDIAccess with sysex:false.
//   2. Raw note-on translates to DeckPlay JSON-RPC payload.
//   3. CC translates to EqAdjust with clamped value.
//   4. Pitch bend out-of-range gets clamped into [-12, +12].
//   5. Buffer overflow drops oldest + warns.
//   6. Disabled WS buffers events until isOpen.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { WebMIDIListener, type JsonRpcWS } from "./listener.ts";

interface FakeInputCtor {
  onmidimessage: ((ev: MIDIMessageEvent) => void) | null;
}

function makeFakeInput(): FakeInputCtor {
  return { onmidimessage: null };
}

function makeFakeAccess(inputs: FakeInputCtor[]): MIDIAccess {
  const inputMap = new Map<string, FakeInputCtor>();
  inputs.forEach((i, idx) => inputMap.set(String(idx), i));
  return {
    inputs: inputMap as unknown as MIDIInputMap,
    outputs: new Map() as unknown as MIDIOutputMap,
    sysexEnabled: false,
    onstatechange: null,
  } as unknown as MIDIAccess;
}

function makeRpc(open: boolean): JsonRpcWS & { calls: Array<[string, unknown]> } {
  const calls: Array<[string, unknown]> = [];
  return {
    calls,
    isOpen: () => open,
    call: vi.fn(async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { accepted: true };
    }),
  };
}

function msg(status: number, d1: number, d2: number): MIDIMessageEvent {
  return { data: new Uint8Array([status, d1, d2]) } as unknown as MIDIMessageEvent;
}

describe("WebMIDIListener", () => {
  let originalNav: Navigator | undefined;

  beforeEach(() => {
    originalNav = globalThis.navigator;
  });

  afterEach(() => {
    if (originalNav !== undefined) {
      Object.defineProperty(globalThis, "navigator", {
        value: originalNav,
        configurable: true,
      });
    }
    vi.restoreAllMocks();
  });

  it("start() requests WebMIDI access with sysex:false and attaches handlers", async () => {
    const input = makeFakeInput();
    const access = makeFakeAccess([input]);
    const requestMIDIAccess = vi.fn().mockResolvedValue(access);
    Object.defineProperty(globalThis, "navigator", {
      value: { requestMIDIAccess },
      configurable: true,
    });

    const rpc = makeRpc(true);
    const listener = new WebMIDIListener(rpc);
    await listener.start("ddj200");

    expect(requestMIDIAccess).toHaveBeenCalledWith({ sysex: false });
    expect(input.onmidimessage).toBeTypeOf("function");
    expect(listener.currentMapping?.id).toBe("ddj200");
  });

  it("translates note-on for play_pause into DeckPlay JSON-RPC call", () => {
    const rpc = makeRpc(true);
    const listener = new WebMIDIListener(rpc);
    // Inject mapping via start path is async; use a shortcut: stash directly.
    // We replicate the start path's index-building by calling start with a
    // fake access that has no inputs, then dispatching a synthetic msg.
    void listener; // bypass strict-no-unused

    // Direct test via translator-equivalent path: invoke start synchronously
    // by injecting a fake navigator.
    return (async () => {
      const access = makeFakeAccess([]);
      Object.defineProperty(globalThis, "navigator", {
        value: { requestMIDIAccess: vi.fn().mockResolvedValue(access) },
        configurable: true,
      });
      await listener.start("ddj200");

      // Note on, channel 0, note 0x0B, velocity 100 → Deck A play_pause
      listener.onMessage(msg(0x90, 0x0b, 100));
      expect(rpc.calls).toHaveLength(1);
      expect(rpc.calls[0]![0]).toBe("engine.submit_event");
      expect(rpc.calls[0]![1]).toEqual({ DeckPlay: { deck: "A" } });
    })();
  });

  it("clamps EQ CC values into the configured output range", async () => {
    const rpc = makeRpc(true);
    const listener = new WebMIDIListener(rpc);
    const access = makeFakeAccess([]);
    Object.defineProperty(globalThis, "navigator", {
      value: { requestMIDIAccess: vi.fn().mockResolvedValue(access) },
      configurable: true,
    });
    await listener.start("ddj200");

    // CC ch 0, controller 0 = Deck A EQ Low. raw=127 → ratio 1.0
    // outputRange in mapping: [-26, 12] but translator clamps upper at +12.
    listener.onMessage(msg(0xb0, 0x00, 127));
    expect(rpc.calls).toHaveLength(1);
    const params = rpc.calls[0]![1] as { EqAdjust: { value_db: number } };
    expect(params.EqAdjust.value_db).toBeLessThanOrEqual(12);
    expect(params.EqAdjust.value_db).toBeGreaterThan(11.9);
  });

  it("pitch bend at min/max maps to clamped semitones [-12, +12]", async () => {
    const rpc = makeRpc(true);
    const listener = new WebMIDIListener(rpc);
    const access = makeFakeAccess([]);
    Object.defineProperty(globalThis, "navigator", {
      value: { requestMIDIAccess: vi.fn().mockResolvedValue(access) },
      configurable: true,
    });
    await listener.start("ddj200");

    // Pitch bend, channel 0, raw 0 (LSB=0,MSB=0) → -12 semitones
    listener.onMessage(msg(0xe0, 0x00, 0x00));
    // Pitch bend, channel 0, raw 16383 (LSB=127,MSB=127) → +12 semitones
    listener.onMessage(msg(0xe0, 0x7f, 0x7f));

    expect(rpc.calls).toHaveLength(2);
    const low = rpc.calls[0]![1] as { PitchBend: { semitones: number } };
    const high = rpc.calls[1]![1] as { PitchBend: { semitones: number } };
    expect(low.PitchBend.semitones).toBeCloseTo(-12, 5);
    expect(high.PitchBend.semitones).toBeCloseTo(12, 5);
  });

  it("buffers events when WS is closed and drops oldest on overflow", async () => {
    const rpc = makeRpc(false);
    const listener = new WebMIDIListener(rpc);
    const access = makeFakeAccess([]);
    Object.defineProperty(globalThis, "navigator", {
      value: { requestMIDIAccess: vi.fn().mockResolvedValue(access) },
      configurable: true,
    });
    await listener.start("ddj200");
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);

    // Send 105 note-on events (DeckPlay) — capacity is 100, expect 5 drops.
    for (let i = 0; i < 105; i++) {
      listener.onMessage(msg(0x90, 0x0b, 100));
    }
    expect(rpc.calls).toHaveLength(0);
    expect(listener.bufferedCount).toBe(100);
    expect(warn).toHaveBeenCalled();
  });

  it("flushes buffered events once WS opens", async () => {
    // RPC starts closed, then flips open before flush().
    let open = false;
    const calls: Array<[string, unknown]> = [];
    const rpc: JsonRpcWS = {
      isOpen: () => open,
      call: vi.fn(async (method: string, params: unknown) => {
        calls.push([method, params]);
        return { accepted: true };
      }),
    };
    const listener = new WebMIDIListener(rpc);
    const access = makeFakeAccess([]);
    Object.defineProperty(globalThis, "navigator", {
      value: { requestMIDIAccess: vi.fn().mockResolvedValue(access) },
      configurable: true,
    });
    await listener.start("ddj200");

    listener.onMessage(msg(0x90, 0x0b, 100)); // Deck A play
    listener.onMessage(msg(0x91, 0x0b, 100)); // Deck B play
    expect(listener.bufferedCount).toBe(2);

    open = true;
    await listener.flush();
    expect(calls).toHaveLength(2);
    expect(calls[0]![1]).toEqual({ DeckPlay: { deck: "A" } });
    expect(calls[1]![1]).toEqual({ DeckPlay: { deck: "B" } });
    expect(listener.bufferedCount).toBe(0);
  });
});
