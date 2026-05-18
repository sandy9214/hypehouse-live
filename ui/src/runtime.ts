// Runtime-environment shim.
//
// HypeHouse Live runs in two surfaces:
//
//   1. Inside the Tauri desktop shell — bridge URL + bearer token are
//      fetched at startup via Tauri's `invoke()` API (see
//      `tauri/src/commands.rs`). The token is generated per app launch
//      so each session has a unique credential.
//
//   2. In a plain browser (Vite dev server, manual engine) — neither
//      `invoke` nor `__TAURI_INTERNALS__` exist. We fall back to the
//      Vite-provided env (`VITE_BRIDGE_URL`) and a development token
//      (`dev-token`) which the engine accepts when its dev profile is
//      active.
//
// The detection is kept defensive: any failure in the Tauri code path
// (missing global, invoke throwing, malformed return value) falls back
// silently to the browser defaults. This keeps the dev-only path
// resilient even if Tauri ships a breaking rename.

const DEFAULT_DEV_URL = "ws://127.0.0.1:8765";
const DEFAULT_DEV_TOKEN = "dev-token";

// Tauri v2 exposes globals via `__TAURI_INTERNALS__`. v1 used
// `__TAURI__`. We check both for safety; v1 will never carry our
// commands but the detection short-circuits to fallback cleanly.
interface TauriGlobals {
  __TAURI_INTERNALS__?: unknown;
  __TAURI__?: unknown;
}

/** Whether we appear to be running inside a Tauri desktop window. */
export function isTauri(): boolean {
  if (typeof window === "undefined") return false;
  const w = window as unknown as TauriGlobals;
  return Boolean(w.__TAURI_INTERNALS__ ?? w.__TAURI__);
}

// Shape of the `invoke` function we depend on. Typed minimally so we
// don't pull `@tauri-apps/api` into the UI's package.json for browser-
// mode builds — Tauri ships the helper into the window scope at
// runtime, but the global typings live in the `@tauri-apps/api`
// package. We declare a local subset.
type Invoke = <T>(cmd: string) => Promise<T>;

interface TauriWithCore {
  core?: { invoke?: Invoke };
  invoke?: Invoke;
}

function resolveInvoke(): Invoke | null {
  if (typeof window === "undefined") return null;
  const w = window as unknown as { __TAURI__?: TauriWithCore; __TAURI_INTERNALS__?: TauriWithCore };
  const v2 = w.__TAURI_INTERNALS__;
  if (v2 && typeof v2 === "object") {
    const candidate = (v2 as TauriWithCore).invoke;
    if (typeof candidate === "function") return candidate;
  }
  const v1 = w.__TAURI__;
  if (v1 && typeof v1 === "object") {
    const inner = v1.core?.invoke ?? v1.invoke;
    if (typeof inner === "function") return inner;
  }
  return null;
}

/** Bridge URL the JsonRpcWS client should connect to. */
export async function getBridgeUrl(): Promise<string> {
  const invoke = resolveInvoke();
  if (invoke) {
    try {
      const url = await invoke<string>("get_bridge_url");
      if (typeof url === "string" && url.length > 0) return url;
    } catch {
      // fall through to browser-mode default
    }
  }
  const envUrl = readEnv("VITE_BRIDGE_URL");
  return envUrl ?? DEFAULT_DEV_URL;
}

/** Bearer token to put in the `auth.hello` JSON-RPC handshake. */
export async function getBridgeToken(): Promise<string> {
  const invoke = resolveInvoke();
  if (invoke) {
    try {
      const tok = await invoke<string>("get_bridge_token");
      if (typeof tok === "string" && tok.length > 0) return tok;
    } catch {
      // fall through
    }
  }
  const envTok = readEnv("VITE_BRIDGE_TOKEN");
  return envTok ?? DEFAULT_DEV_TOKEN;
}

// Vite exposes env vars via `import.meta.env`; this helper isolates the
// access so tests can stub it and so non-Vite consumers (Vitest with
// jsdom) don't blow up if the field is missing.
function readEnv(name: string): string | null {
  try {
    const env = (import.meta as unknown as { env?: Record<string, string> }).env;
    if (env && typeof env[name] === "string" && env[name].length > 0) {
      return env[name];
    }
  } catch {
    // import.meta may not be defined in some test runners; ignore.
  }
  return null;
}
