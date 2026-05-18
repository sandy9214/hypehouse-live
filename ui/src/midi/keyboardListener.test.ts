// vitest tests for KeyboardListener.
//
// Synthetic KeyboardEvent dispatch on a fake EventTarget; assert
// submit_event calls + clamped pitch + crossfader nudge state.

import { describe, expect, it, vi } from "vitest";

import { KeyboardListener } from "./keyboardListener.ts";
import type { JsonRpcWS } from "./listener.ts";

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

function keyEvent(key: string, init: KeyboardEventInit = {}): KeyboardEvent {
  // Minimal shape; the listener only reads .key and .repeat.
  return { key, repeat: false, ...init } as unknown as KeyboardEvent;
}

describe("KeyboardListener", () => {
  it("'q' triggers Deck A play, 'p' triggers Deck B play", () => {
    const rpc = makeRpc(true);
    const kb = new KeyboardListener(rpc);

    kb.onKey(keyEvent("q"));
    kb.onKey(keyEvent("p"));

    expect(rpc.calls).toHaveLength(2);
    expect(rpc.calls[0]![1]).toEqual({ DeckPlay: { deck: "A" } });
    expect(rpc.calls[1]![1]).toEqual({ DeckPlay: { deck: "B" } });
  });

  it("digit keys 1..8 trigger Deck A hot cues 0..7", () => {
    const rpc = makeRpc(true);
    const kb = new KeyboardListener(rpc);

    for (let i = 1; i <= 8; i++) {
      kb.onKey(keyEvent(String(i)));
    }
    expect(rpc.calls).toHaveLength(8);
    for (let i = 0; i < 8; i++) {
      expect(rpc.calls[i]![1]).toEqual({
        HotCueTrigger: { deck: "A", slot: i },
      });
    }
  });

  it("'z' and 'x' nudge Deck A pitch by -0.5 / +0.5 with clamping", () => {
    const rpc = makeRpc(true);
    const kb = new KeyboardListener(rpc);

    // 30 presses of 'x' (+0.5 each) → should clamp at +12 (24 presses = +12).
    for (let i = 0; i < 30; i++) kb.onKey(keyEvent("x"));
    const last = rpc.calls.at(-1)![1] as {
      PitchBend: { deck: string; semitones: number };
    };
    expect(last.PitchBend.semitones).toBeLessThanOrEqual(12);
    expect(last.PitchBend.deck).toBe("A");
  });

  it("',' and '.' nudge crossfader by ±0.05 from 0.5 baseline", () => {
    const rpc = makeRpc(true);
    const kb = new KeyboardListener(rpc);

    kb.onKey(keyEvent(",")); // 0.5 - 0.05 = 0.45
    kb.onKey(keyEvent(".")); // 0.45 + 0.05 = 0.50

    expect(rpc.calls).toHaveLength(2);
    const first = rpc.calls[0]![1] as { Crossfader: { value: number } };
    const second = rpc.calls[1]![1] as { Crossfader: { value: number } };
    expect(first.Crossfader.value).toBeCloseTo(0.45, 5);
    expect(second.Crossfader.value).toBeCloseTo(0.5, 5);
  });

  it("ignores key repeats (no hot-cue spam from held key)", () => {
    const rpc = makeRpc(true);
    const kb = new KeyboardListener(rpc);

    kb.onKey(keyEvent("1"));
    kb.onKey(keyEvent("1", { repeat: true }));
    kb.onKey(keyEvent("1", { repeat: true }));

    expect(rpc.calls).toHaveLength(1);
  });

  it("buffers events when WS is closed and flushes once open", async () => {
    let open = false;
    const calls: Array<[string, unknown]> = [];
    const rpc: JsonRpcWS = {
      isOpen: () => open,
      call: vi.fn(async (method: string, params: unknown) => {
        calls.push([method, params]);
        return { accepted: true };
      }),
    };
    const kb = new KeyboardListener(rpc);

    kb.onKey(keyEvent("q"));
    kb.onKey(keyEvent("p"));
    expect(kb.bufferedCount).toBe(2);
    expect(calls).toHaveLength(0);

    open = true;
    await kb.flush();
    expect(calls).toHaveLength(2);
    expect(kb.bufferedCount).toBe(0);
  });

  it("start() attaches to a target, stop() detaches", () => {
    const rpc = makeRpc(true);
    const kb = new KeyboardListener(rpc);
    const listeners = new Map<string, EventListenerOrEventListenerObject>();
    const target: EventTarget = {
      addEventListener: vi.fn((type: string, listener: EventListenerOrEventListenerObject) => {
        listeners.set(type, listener);
      }),
      removeEventListener: vi.fn((type: string) => {
        listeners.delete(type);
      }),
      dispatchEvent: vi.fn(() => true),
    } as unknown as EventTarget;

    kb.start(target);
    expect(listeners.has("keydown")).toBe(true);

    kb.stop();
    expect(listeners.has("keydown")).toBe(false);
  });
});
