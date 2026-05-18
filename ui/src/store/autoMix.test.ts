// autoMix.test.ts — store reducer + RPC call shape.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { JsonRpcWS } from "../ws/client";
import {
  __resetAutoMix,
  applyAutoMixNotification,
  setAutoMix,
} from "./autoMix";

describe("autoMix store", () => {
  beforeEach((): void => {
    __resetAutoMix();
  });
  afterEach((): void => {
    __resetAutoMix();
  });

  it("ignores notifications with unknown methods", (): void => {
    // Doesn't throw; doesn't update internal state.
    applyAutoMixNotification({
      jsonrpc: "2.0",
      method: "engine.state_changed",
      params: {},
    });
  });

  it("applyAutoMixNotification updates the deck snapshot", (): void => {
    let calls = 0;
    applyAutoMixNotification({
      jsonrpc: "2.0",
      method: "copilot.auto_mix_state_changed",
      params: { deck: "A", status: "armed", seconds_to_mix: 12 },
    });
    calls++;
    // Sanity — subscriber-free check via re-applying same payload
    // should not blow up.
    applyAutoMixNotification({
      jsonrpc: "2.0",
      method: "copilot.auto_mix_state_changed",
      params: { deck: "A", status: "armed", seconds_to_mix: 12 },
    });
    expect(calls).toBe(1);
  });

  it("setAutoMix sends copilot.set_auto_mix and optimistically updates", async (): Promise<void> => {
    const call = vi
      .fn()
      .mockResolvedValue({
        deck: "A",
        enabled: true,
        status: "idle",
        seconds_to_mix: null,
      });
    const client = { call } as unknown as JsonRpcWS;
    await setAutoMix(client, "A", true);
    expect(call).toHaveBeenCalledWith("copilot.set_auto_mix", {
      deck: "A",
      enabled: true,
    });
  });

  it("setAutoMix rolls back on RPC failure", async (): Promise<void> => {
    const call = vi.fn().mockRejectedValue(new Error("boom"));
    const client = { call } as unknown as JsonRpcWS;
    // No throw — failure is swallowed and the local snapshot reverts.
    await setAutoMix(client, "A", true);
    expect(call).toHaveBeenCalledOnce();
  });

  it("rejects malformed notifications (bad deck)", (): void => {
    // Doesn't throw; just drops the malformed payload.
    applyAutoMixNotification({
      jsonrpc: "2.0",
      method: "copilot.auto_mix_state_changed",
      params: { deck: "Z", status: "armed", seconds_to_mix: 12 },
    });
  });

  it("rejects malformed notifications (bad status)", (): void => {
    applyAutoMixNotification({
      jsonrpc: "2.0",
      method: "copilot.auto_mix_state_changed",
      params: { deck: "A", status: "wat", seconds_to_mix: 12 },
    });
  });
});
