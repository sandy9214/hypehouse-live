// Shared types for MIDI + keyboard mappings.
//
// Both `mappings/ddj200.json` (hardware) and `mappings/keyboard.json`
// (browser fallback) deserialize to the same `MidiMapping` shape so the
// translator in `listener.ts` / `keyboardListener.ts` doesn't fork by
// source. The Rust-side mapping (engine/src/midi/mapping.rs) is
// intentionally similar but lives separately — we don't import across
// the language boundary at runtime.

export type DeckId = "A" | "B";
export type EqBand = "Low" | "Mid" | "High";

/** Clamp spec — both bounds inclusive. Used defensively so a controller
 *  that emits out-of-range CCs cannot drive the engine into invalid
 *  states (CLAUDE.md: "Bypass MIDI input validation" is a "must NOT"). */
export interface ClampSpec {
  min: number;
  max: number;
}

/** A single binding from a MIDI or keyboard input to a `submit_event`
 *  call. Either `noteOn`+`note` (note-on triggers), `cc`+`controller`
 *  (continuous control), `pitchBend`+`channel` (14-bit pitch wheel),
 *  or `key` (keyboard event.key) identifies the input.
 *
 *  `action` is the high-level intent; the translator turns it into an
 *  `EventKind` payload. `deck` is required for per-deck actions and
 *  omitted for master actions (e.g. crossfader).
 */
export interface MidiBinding {
  /** Trigger source — exactly one of `noteOn`/`cc`/`pitchBend`/`key`. */
  noteOn?: { channel: number; note: number };
  cc?: { channel: number; controller: number };
  pitchBend?: { channel: number };
  key?: string;

  /** High-level action — translator maps to EventKind. */
  action: MidiAction;
  deck?: DeckId;
  /** Hot-cue slot 0..7 when action === "hot_cue". */
  slot?: number;
  /** EQ band when action === "eq". */
  band?: EqBand;

  /** Range in source units (0..127 for CC, 0..16383 for pitch_bend,
   *  -1..1 for keyboard delta). Defaults provided in the translator. */
  inputRange?: ClampSpec;
  /** Range in engine units (semitones for pitch, dB for EQ, 0..1 for
   *  faders). Defensive clamps applied AFTER range mapping. */
  outputRange?: ClampSpec;
  /** Keyboard-only: delta added to current state on key-press. */
  delta?: number;
}

export type MidiAction =
  | "play_pause"
  | "play"
  | "pause"
  | "cue"
  | "hot_cue"
  | "pitch"
  | "pitch_delta"
  | "eq"
  | "crossfader"
  | "crossfader_delta"
  | "loop_in"
  | "loop_out"
  | "loop_exit"
  | "copilot_toggle"
  | "take_over";

export interface MidiMapping {
  id: string;
  deviceNameMatch?: string;
  bindings: MidiBinding[];
}
