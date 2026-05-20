// sessionInfo.test.ts — pure-function coverage for the cloud-sync
// status helpers. Hook + store wiring are covered indirectly by
// AboutPanel.test.tsx; this file pins down the
// `formatRelativeMicros` formatting contract so future engine-side
// changes to micros precision don't silently break the UI label.

import { describe, expect, it, vi } from "vitest";
import {
  __resetStemsStatus,
  fetchStemsStatus,
  formatCountdownMicros,
  formatRelativeMicros,
} from "./sessionInfo";
import type { JsonRpcWS } from "../ws/client";

const MS = 1_000;
const MICROS_PER_MS = 1_000;

describe("formatRelativeMicros", () => {
  it("returns 'never' for the daemon's pre-tick sentinel (0)", () => {
    expect(formatRelativeMicros(0)).toBe("never");
  });

  it("returns 'never' for negative or non-finite values", () => {
    expect(formatRelativeMicros(-1)).toBe("never");
    expect(formatRelativeMicros(Number.NaN)).toBe("never");
    expect(formatRelativeMicros(Number.POSITIVE_INFINITY)).toBe("never");
  });

  it("returns 'just now' inside the 5-second floor", () => {
    const now = 10_000_000 * MS;
    const micros = (now - 2 * MS) * MICROS_PER_MS;
    expect(formatRelativeMicros(micros, now)).toBe("just now");
  });

  it("handles clock-skew (engine micros in the future)", () => {
    const now = 10_000_000 * MS;
    const future = (now + 30 * MS) * MICROS_PER_MS;
    expect(formatRelativeMicros(future, now)).toBe("just now");
  });

  it("formats seconds in the 5-59s band", () => {
    const now = 10_000_000 * MS;
    expect(
      formatRelativeMicros((now - 12 * MS) * MICROS_PER_MS, now),
    ).toBe("12s ago");
    expect(
      formatRelativeMicros((now - 59 * MS) * MICROS_PER_MS, now),
    ).toBe("59s ago");
  });

  it("formats minutes in the 1-59m band", () => {
    const now = 10_000_000 * MS;
    expect(
      formatRelativeMicros((now - 60 * MS) * MICROS_PER_MS, now),
    ).toBe("1m ago");
    expect(
      formatRelativeMicros((now - 7 * 60 * MS) * MICROS_PER_MS, now),
    ).toBe("7m ago");
  });

  it("formats hours in the 1-23h band", () => {
    const now = 10_000_000 * MS;
    expect(
      formatRelativeMicros((now - 60 * 60 * MS) * MICROS_PER_MS, now),
    ).toBe("1h ago");
    expect(
      formatRelativeMicros((now - 23 * 60 * 60 * MS) * MICROS_PER_MS, now),
    ).toBe("23h ago");
  });

  it("rolls over to days at 24h+ for relative formatter", () => {
    const now = 10_000_000 * MS;
    expect(
      formatRelativeMicros(
        (now - 24 * 60 * 60 * MS) * MICROS_PER_MS,
        now,
      ),
    ).toBe("1d ago");
    expect(
      formatRelativeMicros(
        (now - 9 * 24 * 60 * 60 * MS) * MICROS_PER_MS,
        now,
      ),
    ).toBe("9d ago");
  });
});

describe("formatCountdownMicros", () => {
  it("returns empty string for the daemon's pre-tick sentinel (0)", () => {
    expect(formatCountdownMicros(0)).toBe("");
  });

  it("returns empty string for negative or non-finite values", () => {
    expect(formatCountdownMicros(-1)).toBe("");
    expect(formatCountdownMicros(Number.NaN)).toBe("");
    expect(formatCountdownMicros(Number.POSITIVE_INFINITY)).toBe("");
  });

  it("returns 'due' when the schedule has slipped into the past", () => {
    const now = 10_000_000 * MS;
    expect(
      formatCountdownMicros((now - 3 * MS) * MICROS_PER_MS, now),
    ).toBe("due");
  });

  it("formats sub-minute waits as seconds", () => {
    const now = 10_000_000 * MS;
    expect(
      formatCountdownMicros((now + 12_000) * MICROS_PER_MS, now),
    ).toBe("12s");
    expect(
      formatCountdownMicros((now + 1_000) * MICROS_PER_MS, now),
    ).toBe("1s");
  });

  it("formats minute-scale waits as 'Xm Ys'", () => {
    const now = 10_000_000 * MS;
    expect(
      formatCountdownMicros((now + 75_000) * MICROS_PER_MS, now),
    ).toBe("1m 15s");
    expect(
      formatCountdownMicros((now + 120_000) * MICROS_PER_MS, now),
    ).toBe("2m");
  });

  it("handles the 10-minute backoff cap", () => {
    const now = 10_000_000 * MS;
    // 600s exact → "10m"
    expect(
      formatCountdownMicros((now + 600_000) * MICROS_PER_MS, now),
    ).toBe("10m");
  });
});

const makeClient = (
  responder: (method: string) => Promise<unknown>,
): JsonRpcWS =>
  ({ call: vi.fn(responder) }) as unknown as JsonRpcWS;

describe("fetchStemsStatus", () => {
  it("parses a well-formed payload into the StemsStatus store", async () => {
    __resetStemsStatus();
    const client = makeClient(async (m: string) => {
      if (m === "library.stems_status") {
        return { ready: 7, pending: 3, failed: 1, none: 22 };
      }
      throw new Error("unmocked");
    });
    const status = await fetchStemsStatus(client);
    expect(status).toEqual({
      ready: 7,
      pending: 3,
      failed: 1,
      none: 22,
    });
  });

  it("falls back to all-zero defaults on RPC throw", async () => {
    __resetStemsStatus();
    const client = makeClient(async () => {
      throw new Error("WS hangup");
    });
    const status = await fetchStemsStatus(client);
    expect(status).toEqual({ ready: 0, pending: 0, failed: 0, none: 0 });
  });

  it("falls back to defaults when the wire shape is missing a key", async () => {
    __resetStemsStatus();
    const client = makeClient(async () => ({
      // Missing `none` — typeof guard must reject the whole payload
      // rather than silently filling 0 (the wire contract is "all 4
      // keys always present"; partial responses are bugs).
      ready: 5,
      pending: 0,
      failed: 0,
    }));
    const status = await fetchStemsStatus(client);
    expect(status).toEqual({ ready: 0, pending: 0, failed: 0, none: 0 });
  });

  it("falls back to defaults when a value is non-numeric", async () => {
    __resetStemsStatus();
    const client = makeClient(async () => ({
      ready: "7", // string, not number
      pending: 0,
      failed: 0,
      none: 0,
    }));
    const status = await fetchStemsStatus(client);
    expect(status).toEqual({ ready: 0, pending: 0, failed: 0, none: 0 });
  });
});
