// Spec-shaped localStorage polyfill for tests.
//
// Vitest 4 + jsdom 29 ship a non-spec `window.localStorage` (plain object,
// no Storage prototype + missing methods). Patch in a Map-backed
// implementation so the modules under test can round-trip
// `getItem` / `setItem` / `removeItem` / `clear` / `key` / `length`.
//
// Call once at the TOP of a test file (module-init scope) BEFORE the
// imports that depend on localStorage:
//
//   import { installLocalStoragePolyfill } from "../test-utils/localStoragePolyfill";
//   installLocalStoragePolyfill();
//
//   import { stuff } from "./module";   // safe — uses the polyfill
//
// Idempotent — repeated calls keep replacing the value; tests that need
// a fresh store should call `installLocalStoragePolyfill()` again in
// `beforeEach`.

interface PolyfillStorage {
  getItem: (k: string) => string | null;
  setItem: (k: string, v: string) => void;
  removeItem: (k: string) => void;
  clear: () => void;
  key: (i: number) => string | null;
  readonly length: number;
}

export const installLocalStoragePolyfill = (): void => {
  const store = new Map<string, string>();
  const polyfill: PolyfillStorage = {
    getItem: (k) => (store.has(k) ? (store.get(k) as string) : null),
    setItem: (k, v) => {
      store.set(k, String(v));
    },
    removeItem: (k) => {
      store.delete(k);
    },
    clear: () => {
      store.clear();
    },
    key: (i) => Array.from(store.keys())[i] ?? null,
    get length(): number {
      return store.size;
    },
  };
  Object.defineProperty(window, "localStorage", {
    configurable: true,
    writable: true,
    value: polyfill,
  });
};
