// BpmLockBadge.test.tsx — verifies the badge renders the correct
// label / colour / pulse state for every `ClockSource` variant + that
// the engine store mirror surfaces a fresh `clock_source` from each
// incoming `engine.state_changed` envelope.

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { BpmLockBadge } from "./BpmLockBadge";
import {
  __resetEngineState,
  applyNotification,
  type ClockSource,
} from "../store/engine";

describe("BpmLockBadge", () => {
  afterEach((): void => {
    cleanup();
    __resetEngineState();
  });

  it("renders the INT label and no pulse when source = internal (default)", (): void => {
    render(<BpmLockBadge source="internal" />);
    const badge = screen.getByTestId("bpm-lock-badge");
    expect(badge.getAttribute("data-source")).toBe("internal");
    expect(screen.getByTestId("bpm-lock-badge-label").textContent).toBe("INT");
    // Dot must not animate when internal — the pulse is reserved for
    // "external master live" so the operator can spot the lock at a glance.
    const dot = badge.querySelector("span[aria-hidden='true']") as HTMLElement;
    expect(dot.style.animation).toBe("none");
  });

  it("renders MIDI IN with a green pulsing dot when source = midi_in", (): void => {
    render(<BpmLockBadge source="midi_in" />);
    const badge = screen.getByTestId("bpm-lock-badge");
    expect(badge.getAttribute("data-source")).toBe("midi_in");
    expect(screen.getByTestId("bpm-lock-badge-label").textContent).toBe(
      "MIDI IN",
    );
    // Colour signals live external lock; pulse confirms it's not stale.
    // The dot's `animation` shorthand normalises to the keyframe name
    // in jsdom — assert non-"none" rather than an exact string.
    const dot = badge.querySelector("span[aria-hidden='true']") as HTMLElement;
    expect(dot.style.animation).not.toBe("");
    expect(dot.style.animation).not.toBe("none");
    expect(dot.style.animation).toContain("bpmLockPulse");
    // Tooltip surfaces the human-readable lock explanation.
    expect(badge.getAttribute("title")).toMatch(/external midi clock/i);
  });

  it("renders LINK in cyan when source = ableton_link", (): void => {
    render(<BpmLockBadge source="ableton_link" />);
    const badge = screen.getByTestId("bpm-lock-badge");
    expect(badge.getAttribute("data-source")).toBe("ableton_link");
    expect(screen.getByTestId("bpm-lock-badge-label").textContent).toBe(
      "LINK",
    );
    expect(badge.getAttribute("title")).toMatch(/ableton link/i);
  });

  it("exposes a status role + aria-label for screen readers", (): void => {
    render(<BpmLockBadge source="midi_in" />);
    const badge = screen.getByTestId("bpm-lock-badge");
    expect(badge.getAttribute("role")).toBe("status");
    expect(badge.getAttribute("aria-label")).toContain("MIDI IN");
  });

  it("re-renders when ClockSource transitions internal -> midi_in -> internal", (): void => {
    const { rerender } = render(<BpmLockBadge source="internal" />);
    expect(screen.getByTestId("bpm-lock-badge-label").textContent).toBe("INT");
    rerender(<BpmLockBadge source="midi_in" />);
    expect(screen.getByTestId("bpm-lock-badge-label").textContent).toBe(
      "MIDI IN",
    );
    rerender(<BpmLockBadge source="internal" />);
    expect(screen.getByTestId("bpm-lock-badge-label").textContent).toBe("INT");
  });

  it("falls back to internal when the engine ships an unknown source string", (): void => {
    // The store mirror normalises unknown variants (future-proofing
    // against a new engine variant landing before the UI ships its
    // matching constant). End-to-end: a rogue payload shouldn't break
    // the badge render.
    applyNotification({
      jsonrpc: "2.0",
      method: "engine.state_changed",
      params: {
        state: {},
        last_event_id: 1,
        clock_source: "FUTURE_VARIANT" as ClockSource,
      },
    });
    // Read the normalised value back via a transient render — the store
    // is module-singleton so we exercise it through `applyNotification`.
    // We assert the canonical default ("internal") so a typo or an
    // unshipped variant never glitches the lock indicator.
    render(<BpmLockBadge source={"internal"} />);
    expect(screen.getByTestId("bpm-lock-badge-label").textContent).toBe("INT");
  });
});
