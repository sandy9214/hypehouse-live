// WebMIDIListener — browser-side MIDI input.
//
// Mirrors the Rust desktop listener in engine/src/midi/listener.rs so
// hypehouse-live works in a plain browser (no Tauri shell). Translates
// raw MIDI status+data1+data2 → JSON-RPC `submit_event` calls over the
// WS bridge defined in engine/src/bridge/rpc.rs.
//
// Buffering: if the WS hasn't finished its handshake yet, we queue up
// to 100 events. On overflow we drop the OLDEST (FIFO) and console.warn
// — losing a play/cue mid-set is unacceptable; losing a stale CC from
// the buffer when we're already saturated is the lesser evil.

import type { MidiBinding, MidiMapping } from "./mapping.ts";
import {
  type DeckState,
  type EventKindPayload,
  freshState,
  translate,
} from "./translator.ts";
import ddj200Mapping from "./mappings/ddj200.json" with { type: "json" };

/** JSON-RPC client interface. The full client lives in another PR
 *  (`ws-bridge-and-rpc`); we depend only on the call signature. */
export interface JsonRpcWS {
  call(method: string, params: unknown): Promise<unknown>;
  isOpen(): boolean;
}

const MAX_BUFFERED_EVENTS = 100;

/** Lookup keys for fast binding dispatch. */
type BindingIndex = {
  notes: Map<string, MidiBinding>; // key: `${channel}:${note}`
  ccs: Map<string, MidiBinding>; // key: `${channel}:${cc}`
  pitchBends: Map<number, MidiBinding>; // key: channel
};

function indexBindings(bindings: MidiBinding[]): BindingIndex {
  const notes = new Map<string, MidiBinding>();
  const ccs = new Map<string, MidiBinding>();
  const pitchBends = new Map<number, MidiBinding>();
  for (const b of bindings) {
    if (b.noteOn) notes.set(`${b.noteOn.channel}:${b.noteOn.note}`, b);
    if (b.cc) ccs.set(`${b.cc.channel}:${b.cc.controller}`, b);
    if (b.pitchBend) pitchBends.set(b.pitchBend.channel, b);
  }
  return { notes, ccs, pitchBends };
}

/** Resolve a mapping id to its bundled mapping JSON. Only `ddj200` is
 *  bundled by default; future mappings will be lazy-loaded. */
async function loadMapping(id: string): Promise<MidiMapping> {
  if (id === "ddj200") return ddj200Mapping as MidiMapping;
  throw new Error(`unknown mapping id: ${id}`);
}

export class WebMIDIListener {
  private readonly rpc: JsonRpcWS;
  private mapping: MidiMapping | null = null;
  private index: BindingIndex | null = null;
  private state: DeckState = freshState();
  private access: MIDIAccess | null = null;
  private buffer: EventKindPayload[] = [];
  private inputHandlers = new WeakMap<MIDIInput, (e: MIDIMessageEvent) => void>();

  constructor(rpc: JsonRpcWS) {
    this.rpc = rpc;
  }

  /** Acquire WebMIDI access, attach handlers to all current + future
   *  inputs. Resolves once permission is granted; rejects if WebMIDI is
   *  unsupported or permission is denied. */
  async start(mappingId: string = "ddj200"): Promise<void> {
    if (typeof navigator === "undefined" || !navigator.requestMIDIAccess) {
      throw new Error("WebMIDI not supported in this environment");
    }
    this.mapping = await loadMapping(mappingId);
    this.index = indexBindings(this.mapping.bindings);

    const access = await navigator.requestMIDIAccess({ sysex: false });
    this.access = access;
    access.onstatechange = (ev): void => this.onStateChange(ev);
    for (const input of access.inputs.values()) {
      this.attachInput(input);
    }
  }

  /** Detach all handlers. Idempotent. */
  stop(): void {
    if (this.access) {
      for (const input of this.access.inputs.values()) {
        this.detachInput(input);
      }
      this.access.onstatechange = null;
      this.access = null;
    }
    this.buffer = [];
  }

  private attachInput(input: MIDIInput): void {
    const handler = (ev: MIDIMessageEvent): void => this.onMessage(ev);
    this.inputHandlers.set(input, handler);
    input.onmidimessage = handler;
  }

  private detachInput(input: MIDIInput): void {
    input.onmidimessage = null;
    this.inputHandlers.delete(input);
  }

  private onStateChange(ev: MIDIConnectionEvent): void {
    const port = ev.port;
    if (!port || port.type !== "input") return;
    if (port.state === "connected") this.attachInput(port as MIDIInput);
    if (port.state === "disconnected") this.detachInput(port as MIDIInput);
  }

  /** Translate a raw MIDI message and submit. Public so unit tests can
   *  invoke directly without a real MIDIAccess. */
  onMessage(ev: MIDIMessageEvent): void {
    const data = ev.data;
    if (!data || data.length < 2 || !this.index) return;
    const status = data[0]!;
    const data1 = data[1]!;
    const data2 = data.length >= 3 ? data[2]! : 0;
    const messageType = status & 0xf0;
    const channel = status & 0x0f;

    let binding: MidiBinding | undefined;
    let rawValue = 0;

    if (messageType === 0x90) {
      // Note On (velocity 0 = note off per MIDI spec)
      binding = this.index.notes.get(`${channel}:${data1}`);
      rawValue = data2;
    } else if (messageType === 0x80) {
      // Note Off — match by note key with velocity 0
      binding = this.index.notes.get(`${channel}:${data1}`);
      rawValue = 0;
    } else if (messageType === 0xb0) {
      // Control Change
      binding = this.index.ccs.get(`${channel}:${data1}`);
      rawValue = data2;
    } else if (messageType === 0xe0) {
      // Pitch Bend — 14-bit
      binding = this.index.pitchBends.get(channel);
      rawValue = (data2 << 7) | data1;
    }

    if (!binding) return;
    const payload = translate(binding, rawValue, this.state);
    if (payload) this.submit(payload);
  }

  /** Fire-and-forget submit_event. If WS not open, buffer. */
  private submit(payload: EventKindPayload): void {
    if (this.rpc.isOpen()) {
      void this.rpc.call("engine.submit_event", payload).catch((err) => {
        // Don't crash the input loop on a single failed call.
        console.warn("submit_event failed", err);
      });
      return;
    }
    if (this.buffer.length >= MAX_BUFFERED_EVENTS) {
      const dropped = this.buffer.shift();
      console.warn(
        `WebMIDIListener: buffer overflow (${MAX_BUFFERED_EVENTS}); dropped oldest event`,
        dropped,
      );
    }
    this.buffer.push(payload);
  }

  /** Flush buffered events. Called when WS opens (caller responsibility). */
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

  /** Test-only accessors. */
  get bufferedCount(): number {
    return this.buffer.length;
  }
  get currentMapping(): MidiMapping | null {
    return this.mapping;
  }
}
