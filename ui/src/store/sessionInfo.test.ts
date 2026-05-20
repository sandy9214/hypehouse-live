// sessionInfo.test.ts — pure-function coverage for the cloud-sync
// status helpers. Hook + store wiring are covered indirectly by
// AboutPanel.test.tsx; this file pins down the
// `formatRelativeMicros` formatting contract so future engine-side
// changes to micros precision don't silently break the UI label.

import { describe, expect, it } from "vitest";
import { formatRelativeMicros } from "./sessionInfo";

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

  it("rolls over to days at 24h+", () => {
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
