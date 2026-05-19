// vitest tests for MappingStore.
//
// jsdom gives us a real localStorage; we wipe it via __resetMappingStore
// in afterEach so no test leaks state into the next. Validation tests
// pin to the same schema enforced by WebMIDIListener.applyMapping so a
// regression on one path can't bypass the other.

import { afterEach, describe, expect, it } from "vitest";

// Vitest 4 + jsdom 29 ships a non-spec localStorage (plain object, no
// Storage prototype + missing methods). Patch in a spec-shaped polyfill
// so the store's guarded reads/writes can round-trip. Mirrors the same
// trick used in Onboarding.test.tsx.
const installLocalStoragePolyfill = (): void => {
  const store = new Map<string, string>();
  const polyfill = {
    getItem: (k: string): string | null =>
      store.has(k) ? (store.get(k) as string) : null,
    setItem: (k: string, v: string): void => { store.set(k, String(v)); },
    removeItem: (k: string): void => { store.delete(k); },
    clear: (): void => store.clear(),
    key: (i: number): string | null => Array.from(store.keys())[i] ?? null,
    get length(): number { return store.size; },
  };
  Object.defineProperty(window, "localStorage", {
    configurable: true, writable: true, value: polyfill,
  });
};
installLocalStoragePolyfill();

import type { MidiMapping } from "./mapping.ts";
import {
  __resetMappingStore,
  classify,
  deleteMapping,
  getActiveMappingName,
  listMappings,
  loadMapping,
  persistMapping,
  setActiveMappingName,
  validateMapping,
} from "./MappingStore.ts";

const CUSTOM_MIDI: MidiMapping = {
  id: "my-mc7000",
  deviceNameMatch: "MC7000",
  bindings: [
    { noteOn: { channel: 0, note: 11 }, action: "play_pause", deck: "A" },
    {
      cc: { channel: 0, controller: 5 },
      action: "eq",
      deck: "A",
      band: "Low",
      inputRange: { min: 0, max: 127 },
      outputRange: { min: -26, max: 12 },
    },
  ],
};

const CUSTOM_KB: MidiMapping = {
  id: "vim-keys",
  bindings: [
    { key: "h", action: "play_pause", deck: "A" },
    { key: "l", action: "play_pause", deck: "B" },
  ],
};

describe("MappingStore", () => {
  afterEach((): void => {
    __resetMappingStore();
  });

  it("listMappings always includes the two built-ins", (): void => {
    const entries = listMappings();
    const names = entries.map((e) => e.name).sort();
    expect(names).toContain("ddj200");
    expect(names).toContain("keyboard");
    expect(entries.find((e) => e.name === "ddj200")?.builtin).toBe(true);
    expect(entries.find((e) => e.name === "keyboard")?.kind).toBe("keyboard");
  });

  it("persistMapping + loadMapping round-trips a custom mapping", (): void => {
    const result = persistMapping(CUSTOM_MIDI.id, CUSTOM_MIDI);
    expect(result.ok).toBe(true);

    const loaded = loadMapping(CUSTOM_MIDI.id);
    expect(loaded).not.toBeNull();
    expect(loaded?.id).toBe(CUSTOM_MIDI.id);
    expect(loaded?.bindings).toHaveLength(2);
    expect(loaded?.bindings[0]!.action).toBe("play_pause");

    const entries = listMappings();
    const entry = entries.find((e) => e.name === CUSTOM_MIDI.id);
    expect(entry).toBeDefined();
    expect(entry?.builtin).toBe(false);
    expect(entry?.kind).toBe("midi");
  });

  it("rejects mappings with unknown actions", (): void => {
    const bad = {
      id: "bad",
      bindings: [{ noteOn: { channel: 0, note: 1 }, action: "explode" }],
    };
    const result = validateMapping(bad);
    expect(result.ok).toBe(false);
    expect(result.error).toContain("action");
  });

  it("rejects mappings with no source on a binding", (): void => {
    const bad = { id: "bad", bindings: [{ action: "play_pause", deck: "A" }] };
    const result = validateMapping(bad);
    expect(result.ok).toBe(false);
    expect(result.error).toContain("exactly one");
  });

  it("rejects mappings declaring multiple sources on one binding", (): void => {
    const bad = {
      id: "bad",
      bindings: [
        {
          noteOn: { channel: 0, note: 0 },
          cc: { channel: 0, controller: 0 },
          action: "play_pause",
          deck: "A",
        },
      ],
    };
    const result = validateMapping(bad);
    expect(result.ok).toBe(false);
  });

  it("rejects MIDI channels out of 0..15 range", (): void => {
    const bad = {
      id: "bad",
      bindings: [{ cc: { channel: 99, controller: 1 }, action: "eq", deck: "A" }],
    };
    expect(validateMapping(bad).ok).toBe(false);
  });

  it("rejects persistence under a built-in name", (): void => {
    const result = persistMapping("ddj200", CUSTOM_MIDI);
    expect(result.ok).toBe(false);
    expect(result.error).toContain("built-in");
  });

  it("rejects persistence of an invalid mapping (no localStorage write)", (): void => {
    const result = persistMapping("borked", { id: "borked", bindings: "nope" });
    expect(result.ok).toBe(false);
    // Verify nothing landed under that name.
    expect(loadMapping("borked")).toBeNull();
  });

  it("classifies keyboard vs midi based on binding source", (): void => {
    expect(classify(CUSTOM_KB)).toBe("keyboard");
    expect(classify(CUSTOM_MIDI)).toBe("midi");
  });

  it("deleteMapping removes a custom mapping and is no-op for built-ins", (): void => {
    persistMapping(CUSTOM_MIDI.id, CUSTOM_MIDI);
    expect(loadMapping(CUSTOM_MIDI.id)).not.toBeNull();
    expect(deleteMapping(CUSTOM_MIDI.id)).toBe(true);
    expect(loadMapping(CUSTOM_MIDI.id)).toBeNull();

    expect(deleteMapping("ddj200")).toBe(false);
    // Built-in still accessible.
    expect(loadMapping("ddj200")).not.toBeNull();
  });

  it("active-mapping selection persists across calls", (): void => {
    expect(getActiveMappingName("midi")).toBe("ddj200");
    setActiveMappingName("midi", "custom-foo");
    expect(getActiveMappingName("midi")).toBe("custom-foo");
    // Keyboard slot independent.
    expect(getActiveMappingName("keyboard")).toBe("keyboard");
    setActiveMappingName("keyboard", "vim-keys");
    expect(getActiveMappingName("keyboard")).toBe("vim-keys");
    expect(getActiveMappingName("midi")).toBe("custom-foo");
  });

  it("loadMapping returns null for tampered localStorage payloads", (): void => {
    // Sneak past persistMapping's validator by writing raw.
    localStorage.setItem(
      "hypehouse:midi-mapping:tampered",
      JSON.stringify({ id: "tampered", bindings: [{ action: "evil" }] }),
    );
    expect(loadMapping("tampered")).toBeNull();
  });
});
