// Tests for the Tauri vs browser-mode detection in runtime.ts.
//
// We poke `window` (jsdom) to simulate each surface and verify the
// helpers return the expected URL/token. The Tauri `invoke` global is
// stubbed by direct assignment — same shape Tauri injects at runtime.

import { afterEach, describe, expect, it, vi } from "vitest";
import { getBridgeToken, getBridgeUrl, isTauri } from "./runtime";

interface TauriShape {
  __TAURI_INTERNALS__?: { invoke?: (cmd: string) => Promise<unknown> };
  __TAURI__?: unknown;
}

afterEach(() => {
  const w = window as unknown as TauriShape;
  delete w.__TAURI_INTERNALS__;
  delete w.__TAURI__;
});

describe("isTauri", () => {
  it("returns false in a vanilla jsdom window", () => {
    expect(isTauri()).toBe(false);
  });

  it("returns true when __TAURI_INTERNALS__ is present", () => {
    (window as unknown as TauriShape).__TAURI_INTERNALS__ = { invoke: vi.fn() };
    expect(isTauri()).toBe(true);
  });
});

describe("getBridgeUrl", () => {
  it("falls back to ws://127.0.0.1:8765 in browser mode without env", async () => {
    expect(await getBridgeUrl()).toMatch(/^ws:\/\/127\.0\.0\.1:8765$/);
  });

  it("uses the Tauri invoke return value when running in the desktop shell", async () => {
    const invoke = vi.fn(async (cmd: string) => {
      expect(cmd).toBe("get_bridge_url");
      return "ws://my-engine.local:1234";
    });
    (window as unknown as TauriShape).__TAURI_INTERNALS__ = { invoke };
    expect(await getBridgeUrl()).toBe("ws://my-engine.local:1234");
    expect(invoke).toHaveBeenCalledOnce();
  });

  it("falls back to default when Tauri invoke throws", async () => {
    (window as unknown as TauriShape).__TAURI_INTERNALS__ = {
      invoke: vi.fn(async () => {
        throw new Error("boom");
      }),
    };
    expect(await getBridgeUrl()).toMatch(/^ws:\/\//);
  });
});

describe("getBridgeToken", () => {
  it("returns the dev fallback in browser mode", async () => {
    const t = await getBridgeToken();
    expect(typeof t).toBe("string");
    expect(t.length).toBeGreaterThan(0);
  });

  it("returns the Tauri-supplied token in desktop mode", async () => {
    (window as unknown as TauriShape).__TAURI_INTERNALS__ = {
      invoke: vi.fn(async (cmd: string) =>
        cmd === "get_bridge_token" ? "deadbeefcafebabe" : "ws://x",
      ),
    };
    expect(await getBridgeToken()).toBe("deadbeefcafebabe");
  });
});
