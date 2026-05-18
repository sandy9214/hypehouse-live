// KeyboardListener — fallback when no MIDI controller is attached.
//
// Listens on window keydown events, translates to `submit_event` calls
// using the same translator + buffer logic as WebMIDIListener.

import type { JsonRpcWS } from "./listener.ts";
import type { MidiBinding, MidiMapping } from "./mapping.ts";
import {
  type DeckState,
  type EventKindPayload,
  freshState,
  translate,
} from "./translator.ts";
import keyboardMapping from "./mappings/keyboard.json" with { type: "json" };

const MAX_BUFFERED_EVENTS = 100;

export class KeyboardListener {
  private readonly rpc: JsonRpcWS;
  private byKey: Map<string, MidiBinding>;
  private state: DeckState = freshState();
  private buffer: EventKindPayload[] = [];
  private handler: EventListener | null = null;
  private target: EventTarget | null = null;

  constructor(rpc: JsonRpcWS, mapping: MidiMapping = keyboardMapping as MidiMapping) {
    this.rpc = rpc;
    this.byKey = new Map();
    for (const b of mapping.bindings) {
      if (b.key !== undefined) this.byKey.set(b.key, b);
    }
  }

  start(target: EventTarget = window): void {
    if (this.handler) return;
    this.target = target;
    this.handler = (ev: Event): void => this.onKey(ev as KeyboardEvent);
    target.addEventListener("keydown", this.handler);
  }

  stop(): void {
    if (this.handler && this.target) {
      this.target.removeEventListener("keydown", this.handler);
    }
    this.handler = null;
    this.target = null;
    this.buffer = [];
  }

  /** Public for unit tests — bypasses event listener. */
  onKey(ev: KeyboardEvent): void {
    // Ignore key repeats (auto-repeat would spam hot-cue triggers).
    if (ev.repeat) return;
    const binding = this.byKey.get(ev.key);
    if (!binding) return;
    const payload = translate(binding, 1, this.state);
    if (payload) this.submit(payload);
  }

  private submit(payload: EventKindPayload): void {
    if (this.rpc.isOpen()) {
      void this.rpc.call("engine.submit_event", payload).catch((err) => {
        console.warn("submit_event failed", err);
      });
      return;
    }
    if (this.buffer.length >= MAX_BUFFERED_EVENTS) {
      const dropped = this.buffer.shift();
      console.warn(
        `KeyboardListener: buffer overflow (${MAX_BUFFERED_EVENTS}); dropped oldest event`,
        dropped,
      );
    }
    this.buffer.push(payload);
  }

  async flush(): Promise<void> {
    if (!this.rpc.isOpen()) return;
    const pending = this.buffer;
    this.buffer = [];
    for (const p of pending) {
      try {
        await this.rpc.call("engine.submit_event", p);
      } catch (err) {
        console.warn("submit_event failed during flush", err);
      }
    }
  }

  get bufferedCount(): number {
    return this.buffer.length;
  }
}
