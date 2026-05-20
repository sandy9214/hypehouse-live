// Unit tests for the output-devices store.
//
// Component-level integration is covered by OutputDevicePicker.test.tsx;
// this file pins down the cache + refetch contract at the store boundary
// so the WS reconnect refetch path (#207-style, applied to output
// devices in this PR) doesn't regress.

import { describe, expect, it, vi } from "vitest";
import type { JsonRpcWS } from "../ws/client";
import {
  __resetOutputDevices,
  fetchOutputDevices,
  refetchOutputDevices,
} from "./outputDevices";

type Responder = (method: string) => Promise<unknown>;

const makeClient = (r: Responder): JsonRpcWS =>
  ({ call: vi.fn(r) }) as unknown as JsonRpcWS;

const seed = (count: number): { devices: { name: string; is_default: boolean }[] } => ({
  devices: Array.from({ length: count }, (_, i): {
    name: string;
    is_default: boolean;
  } => ({
    name: `device-${i}`,
    is_default: i === 0,
  })),
});

describe("fetchOutputDevices cache + refetch", () => {
  it("caches the first successful response — second call is a no-op", async () => {
    __resetOutputDevices();
    let calls = 0;
    const client = makeClient(async (): Promise<unknown> => {
      calls += 1;
      return seed(2);
    });
    const first = await fetchOutputDevices(client);
    expect(first).toHaveLength(2);
    expect(calls).toBe(1);
    // Second fetch hits the cache.
    await fetchOutputDevices(client);
    expect(calls).toBe(1);
  });

  it("refetchOutputDevices bypasses the cache (forces a fresh RPC)", async () => {
    __resetOutputDevices();
    let calls = 0;
    const client = makeClient(async (): Promise<unknown> => {
      calls += 1;
      // Engine returns a different list on the 2nd call — simulates
      // a USB interface plugged in between the two fetches.
      return seed(calls === 1 ? 1 : 3);
    });
    const first = await fetchOutputDevices(client);
    expect(first).toHaveLength(1);
    const second = await refetchOutputDevices(client);
    expect(second).toHaveLength(3);
    expect(calls).toBe(2);
  });

  it("clears to [] on RPC error during refetch", async () => {
    __resetOutputDevices();
    // Seed a known-good list first.
    let ok = true;
    const client = makeClient(async (): Promise<unknown> => {
      if (ok) return seed(2);
      throw new Error("WS hangup");
    });
    const first = await fetchOutputDevices(client);
    expect(first).toHaveLength(2);
    ok = false;
    const second = await refetchOutputDevices(client);
    // Failed refetch must NOT leave stale data visible — store
    // clears to [], matching the pattern from #205's "stale state
    // cleared on fetch error" guarantee.
    expect(second).toHaveLength(0);
  });
});
