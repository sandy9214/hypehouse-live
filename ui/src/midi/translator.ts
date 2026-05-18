// Translator: MidiBinding + raw value → JSON-RPC `submit_event` params.
//
// Pure functions, no I/O. Kept separate from the listener so the
// keyboard and WebMIDI paths can share it and so the unit tests can
// exercise it without stubbing `navigator.requestMIDIAccess`.
//
// Wire format matches engine/src/state.rs EventKind (externally tagged
// serde enum). DeckId is a bare string "A" / "B". EqBand is "Low" /
// "Mid" / "High".

import type { ClampSpec, MidiBinding } from "./mapping.ts";

/** Clamp `x` into [spec.min, spec.max]. NaN-safe: NaN → spec.min. */
export function clamp(x: number, spec: ClampSpec): number {
  if (!Number.isFinite(x)) return spec.min;
  if (x < spec.min) return spec.min;
  if (x > spec.max) return spec.max;
  return x;
}

/** Linearly map `raw` from `input` range to `output` range, then clamp
 *  defensively (controllers can emit junk; engine must clamp — CLAUDE.md). */
export function mapRange(
  raw: number,
  input: ClampSpec,
  output: ClampSpec,
): number {
  const span = input.max - input.min;
  if (span === 0) return output.min;
  const ratio = (raw - input.min) / span;
  const scaled = output.min + ratio * (output.max - output.min);
  return clamp(scaled, output);
}

/** EventKind payload — externally tagged serde format. The bridge's
 *  `engine.submit_event` accepts this shape directly (rpc.rs Bare variant). */
export type EventKindPayload = Record<string, unknown>;

/** State the translator needs to handle delta-style bindings (keyboard
 *  pitch nudges, crossfader nudges). Continuous-value bindings (pitch
 *  bend, EQ CCs) ignore this and use the raw input directly. */
export interface DeckState {
  pitchA: number;
  pitchB: number;
  crossfader: number;
}

export function freshState(): DeckState {
  return { pitchA: 0, pitchB: 0, crossfader: 0.5 };
}

/** Translate a binding + raw value into a `submit_event` payload, or
 *  `null` if the binding has no effect (e.g. note-off treated as no-op).
 *  `rawValue` semantics:
 *    - noteOn: velocity (1..127). 0 is treated as note-off → null.
 *    - cc: 0..127 controller value.
 *    - pitchBend: 0..16383 (14-bit, MSB<<7 | LSB).
 *    - key: 1 for keydown, 0 for keyup (we only act on keydown).
 */
export function translate(
  binding: MidiBinding,
  rawValue: number,
  state: DeckState,
): EventKindPayload | null {
  switch (binding.action) {
    case "play_pause":
    case "play":
      if (rawValue === 0) return null;
      return { DeckPlay: { deck: binding.deck ?? "A" } };

    case "pause":
      if (rawValue === 0) return null;
      return { DeckPause: { deck: binding.deck ?? "A" } };

    case "cue":
      if (rawValue === 0) return null;
      return { DeckCue: { deck: binding.deck ?? "A", position_ms: 0 } };

    case "hot_cue":
      if (rawValue === 0) return null;
      return {
        HotCueTrigger: {
          deck: binding.deck ?? "A",
          slot: binding.slot ?? 0,
        },
      };

    case "pitch": {
      const input = binding.inputRange ?? { min: 0, max: 16383 };
      const output = binding.outputRange ?? { min: -12, max: 12 };
      const semitones = mapRange(rawValue, input, output);
      const deck = binding.deck ?? "A";
      if (deck === "A") state.pitchA = semitones;
      else state.pitchB = semitones;
      return { PitchBend: { deck, semitones } };
    }

    case "pitch_delta": {
      if (rawValue === 0) return null;
      const delta = binding.delta ?? 0;
      const output = binding.outputRange ?? { min: -12, max: 12 };
      const deck = binding.deck ?? "A";
      const current = deck === "A" ? state.pitchA : state.pitchB;
      const next = clamp(current + delta, output);
      if (deck === "A") state.pitchA = next;
      else state.pitchB = next;
      return { PitchBend: { deck, semitones: next } };
    }

    case "eq": {
      const input = binding.inputRange ?? { min: 0, max: 127 };
      // EQ output range — clamp upper bound at +12 dB per spec.
      const rawOut = binding.outputRange ?? { min: -26, max: 12 };
      const output: ClampSpec = { min: rawOut.min, max: Math.min(rawOut.max, 12) };
      const value_db = mapRange(rawValue, input, output);
      return {
        EqAdjust: {
          deck: binding.deck ?? "A",
          band: binding.band ?? "Low",
          value_db,
        },
      };
    }

    case "crossfader": {
      const input = binding.inputRange ?? { min: 0, max: 127 };
      const output = binding.outputRange ?? { min: 0, max: 1 };
      const value = mapRange(rawValue, input, output);
      state.crossfader = value;
      return { Crossfader: { value } };
    }

    case "crossfader_delta": {
      if (rawValue === 0) return null;
      const delta = binding.delta ?? 0;
      const output = binding.outputRange ?? { min: 0, max: 1 };
      const value = clamp(state.crossfader + delta, output);
      state.crossfader = value;
      return { Crossfader: { value } };
    }

    case "loop_in":
      if (rawValue === 0) return null;
      return { LoopIn: { deck: binding.deck ?? "A" } };

    case "loop_out":
      if (rawValue === 0) return null;
      return { LoopOut: { deck: binding.deck ?? "A" } };

    case "loop_exit":
      if (rawValue === 0) return null;
      return { LoopExit: { deck: binding.deck ?? "A" } };

    case "copilot_toggle":
      // No state carried client-side — UI sends Engage; engine reducer
      // decides; subsequent toggles will be re-issued when copilot
      // state surfaces via `state_changed`. Conservative default: Engage.
      if (rawValue === 0) return null;
      return { CopilotEngage: { deck: binding.deck ?? "A" } };

    case "take_over":
      if (rawValue === 0) return null;
      return {
        TakeOver: {
          deck: binding.deck ?? "A",
          handoff_until_frame: 0,
        },
      };

    default:
      return null;
  }
}
