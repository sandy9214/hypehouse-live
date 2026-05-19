// MappingStore — persistence + lookup for custom MIDI / keyboard mappings.
//
// Built-in mappings (`ddj200`, `keyboard`) are bundled at compile time;
// the store exposes them alongside any user-imported JSON so the UI can
// list them uniformly. Custom mappings persist in localStorage under
// the `hypehouse:midi-mapping:<name>` key. We deliberately do NOT
// snapshot the bundled mappings into storage — that would let stale
// copies override future engineering tweaks to the canonical files.
//
// Validation is shared with WebMIDIListener.applyMapping; if you change
// the schema check, change it in both places.

import type { MidiAction, MidiMapping } from "./mapping.ts";
import ddj200Mapping from "./mappings/ddj200.json" with { type: "json" };
import keyboardMapping from "./mappings/keyboard.json" with { type: "json" };

const STORAGE_PREFIX = "hypehouse:midi-mapping:";
const ACTIVE_KEY_MIDI = "hypehouse:midi-active:midi";
const ACTIVE_KEY_KEYBOARD = "hypehouse:midi-active:keyboard";

/** Which listener a mapping is bound to. Keyboard mappings carry `key`
 *  bindings; MIDI mappings carry note/cc/pitchBend. We keep these as
 *  separate active slots so importing a custom MIDI map doesn't swap out
 *  the keyboard fallback. */
export type ListenerKind = "midi" | "keyboard";

/** Result of a validation pass. */
export interface ValidationResult {
  ok: boolean;
  error?: string;
}

const VALID_ACTIONS: ReadonlySet<MidiAction> = new Set<MidiAction>([
  "play_pause",
  "play",
  "pause",
  "cue",
  "hot_cue",
  "pitch",
  "pitch_delta",
  "eq",
  "crossfader",
  "crossfader_delta",
  "loop_in",
  "loop_out",
  "loop_exit",
  "copilot_toggle",
  "take_over",
]);

/** Strict structural check for a candidate mapping. Mirrors the shape
 *  expected by `mapping.ts` plus the runtime constraints the translator
 *  assumes (action enum, deck enum, slot/band ranges, integer MIDI
 *  channels/notes, exactly-one input source per binding).
 *
 *  Returns `ok:true` only when every binding passes; on failure the
 *  first offending binding is referenced in `error` so the UI can show
 *  an actionable toast. */
export function validateMapping(candidate: unknown): ValidationResult {
  if (!candidate || typeof candidate !== "object") {
    return { ok: false, error: "mapping must be a JSON object" };
  }
  const obj = candidate as Record<string, unknown>;
  if (typeof obj.id !== "string" || obj.id.length === 0) {
    return { ok: false, error: "mapping.id must be a non-empty string" };
  }
  if (!Array.isArray(obj.bindings)) {
    return { ok: false, error: "mapping.bindings must be an array" };
  }
  for (let i = 0; i < obj.bindings.length; i++) {
    const b = obj.bindings[i] as Record<string, unknown> | null;
    if (!b || typeof b !== "object") {
      return { ok: false, error: `bindings[${i}] must be an object` };
    }
    if (typeof b.action !== "string" || !VALID_ACTIONS.has(b.action as MidiAction)) {
      return {
        ok: false,
        error: `bindings[${i}].action invalid (got ${String(b.action)})`,
      };
    }
    const sourceCount = ["noteOn", "cc", "pitchBend", "key"].filter(
      (k) => b[k] !== undefined,
    ).length;
    if (sourceCount !== 1) {
      return {
        ok: false,
        error: `bindings[${i}] must declare exactly one of noteOn/cc/pitchBend/key`,
      };
    }
    if (b.deck !== undefined && b.deck !== "A" && b.deck !== "B") {
      return { ok: false, error: `bindings[${i}].deck must be 'A' or 'B'` };
    }
    if (b.slot !== undefined) {
      const slot = b.slot as number;
      if (!Number.isInteger(slot) || slot < 0 || slot > 7) {
        return { ok: false, error: `bindings[${i}].slot must be integer 0..7` };
      }
    }
    if (
      b.band !== undefined &&
      b.band !== "Low" &&
      b.band !== "Mid" &&
      b.band !== "High"
    ) {
      return { ok: false, error: `bindings[${i}].band must be Low/Mid/High` };
    }
    if (b.noteOn !== undefined) {
      const n = b.noteOn as { channel?: unknown; note?: unknown };
      if (
        !Number.isInteger(n.channel) ||
        (n.channel as number) < 0 ||
        (n.channel as number) > 15 ||
        !Number.isInteger(n.note) ||
        (n.note as number) < 0 ||
        (n.note as number) > 127
      ) {
        return {
          ok: false,
          error: `bindings[${i}].noteOn requires channel 0..15 + note 0..127`,
        };
      }
    }
    if (b.cc !== undefined) {
      const c = b.cc as { channel?: unknown; controller?: unknown };
      if (
        !Number.isInteger(c.channel) ||
        (c.channel as number) < 0 ||
        (c.channel as number) > 15 ||
        !Number.isInteger(c.controller) ||
        (c.controller as number) < 0 ||
        (c.controller as number) > 127
      ) {
        return {
          ok: false,
          error: `bindings[${i}].cc requires channel 0..15 + controller 0..127`,
        };
      }
    }
    if (b.pitchBend !== undefined) {
      const p = b.pitchBend as { channel?: unknown };
      if (
        !Number.isInteger(p.channel) ||
        (p.channel as number) < 0 ||
        (p.channel as number) > 15
      ) {
        return {
          ok: false,
          error: `bindings[${i}].pitchBend requires channel 0..15`,
        };
      }
    }
    if (b.key !== undefined && typeof b.key !== "string") {
      return { ok: false, error: `bindings[${i}].key must be a string` };
    }
  }
  return { ok: true };
}

/** Classify a mapping as midi or keyboard based on the first binding's
 *  source. Heuristic, but unambiguous in practice — a binding either
 *  comes from a key event or a MIDI event, never both. */
export function classify(mapping: MidiMapping): ListenerKind {
  return mapping.bindings.some((b) => b.key !== undefined) ? "keyboard" : "midi";
}

/** Catalogue entry surfaced to the UI. */
export interface MappingEntry {
  name: string;
  kind: ListenerKind;
  builtin: boolean;
}

const BUILTINS: ReadonlyArray<{ name: string; kind: ListenerKind; mapping: MidiMapping }> = [
  { name: "ddj200", kind: "midi", mapping: ddj200Mapping as MidiMapping },
  { name: "keyboard", kind: "keyboard", mapping: keyboardMapping as MidiMapping },
];

const BUILTIN_BY_NAME: ReadonlyMap<string, MidiMapping> = new Map(
  BUILTINS.map((b) => [b.name, b.mapping] as const),
);

function safeStorage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    // Some test environments throw on bare `localStorage` access.
    return null;
  }
}

/** Persist a custom mapping under `name`. Built-in names are reserved
 *  and cannot be overwritten — that protects the canonical ddj200 /
 *  keyboard configs from accidental clobber. Returns a validation
 *  result so callers can surface the failure verbatim. */
export function persistMapping(name: string, mapping: unknown): ValidationResult {
  if (BUILTIN_BY_NAME.has(name)) {
    return { ok: false, error: `'${name}' is a built-in name; pick another` };
  }
  if (name.length === 0) {
    return { ok: false, error: "mapping name required" };
  }
  const result = validateMapping(mapping);
  if (!result.ok) return result;
  const ls = safeStorage();
  if (!ls) return { ok: false, error: "localStorage unavailable" };
  try {
    ls.setItem(STORAGE_PREFIX + name, JSON.stringify(mapping));
    return { ok: true };
  } catch (e) {
    return { ok: false, error: `failed to persist: ${String(e)}` };
  }
}

/** Resolve a name → mapping. Built-ins win first; falls through to
 *  localStorage. Returns null if not found OR if the persisted blob
 *  fails revalidation (defends against schema drift / hand-edited LS). */
export function loadMapping(name: string): MidiMapping | null {
  const builtin = BUILTIN_BY_NAME.get(name);
  if (builtin) return builtin;
  const ls = safeStorage();
  if (!ls) return null;
  const raw = ls.getItem(STORAGE_PREFIX + name);
  if (raw === null) return null;
  try {
    const parsed = JSON.parse(raw) as unknown;
    const result = validateMapping(parsed);
    if (!result.ok) return null;
    return parsed as MidiMapping;
  } catch {
    return null;
  }
}

/** Remove a custom mapping. Built-ins are no-ops by design. */
export function deleteMapping(name: string): boolean {
  if (BUILTIN_BY_NAME.has(name)) return false;
  const ls = safeStorage();
  if (!ls) return false;
  ls.removeItem(STORAGE_PREFIX + name);
  return true;
}

/** Combined list of built-in + custom mapping names + kind + origin. */
export function listMappings(): MappingEntry[] {
  const entries: MappingEntry[] = BUILTINS.map((b) => ({
    name: b.name,
    kind: b.kind,
    builtin: true,
  }));
  const ls = safeStorage();
  if (!ls) return entries;
  for (let i = 0; i < ls.length; i++) {
    const key = ls.key(i);
    if (!key || !key.startsWith(STORAGE_PREFIX)) continue;
    const name = key.slice(STORAGE_PREFIX.length);
    if (BUILTIN_BY_NAME.has(name)) continue; // can't happen but defensive
    const mapping = loadMapping(name);
    if (!mapping) continue;
    entries.push({ name, kind: classify(mapping), builtin: false });
  }
  return entries;
}

/** Get/set the currently active mapping for a given listener kind. The
 *  WebMIDIListener / KeyboardListener wiring reads this on mount + on
 *  every Reload click; we don't push events from the store on writes
 *  because the UI explicitly calls applyMapping when it wants a swap. */
export function getActiveMappingName(kind: ListenerKind): string {
  const ls = safeStorage();
  const key = kind === "midi" ? ACTIVE_KEY_MIDI : ACTIVE_KEY_KEYBOARD;
  const fallback = kind === "midi" ? "ddj200" : "keyboard";
  if (!ls) return fallback;
  return ls.getItem(key) ?? fallback;
}

export function setActiveMappingName(kind: ListenerKind, name: string): void {
  const ls = safeStorage();
  if (!ls) return;
  const key = kind === "midi" ? ACTIVE_KEY_MIDI : ACTIVE_KEY_KEYBOARD;
  ls.setItem(key, name);
}

/** Test-only — wipe all custom mappings + active selections. Not
 *  exported in production paths because callers should not nuke user
 *  state on accident; tests call it from afterEach. */
export function __resetMappingStore(): void {
  const ls = safeStorage();
  if (!ls) return;
  const toRemove: string[] = [];
  for (let i = 0; i < ls.length; i++) {
    const key = ls.key(i);
    if (!key) continue;
    if (
      key.startsWith(STORAGE_PREFIX) ||
      key === ACTIVE_KEY_MIDI ||
      key === ACTIVE_KEY_KEYBOARD
    ) {
      toRemove.push(key);
    }
  }
  for (const k of toRemove) ls.removeItem(k);
}
