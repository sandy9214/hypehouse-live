// AboutPanel.test.tsx — render + RPC fetch + flag highlighting.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { AboutPanel } from "./AboutPanel";
import {
  __resetSessionInfo,
  __resetSyncStatus,
  __setSessionInfo,
  __setSyncStatus,
  type SessionInfo,
} from "../store/sessionInfo";
import type { JsonRpcWS } from "../ws/client";

const makeClient = (
  call: ((method: string, params?: unknown) => Promise<unknown>) | null = null,
): JsonRpcWS =>
  ({
    call:
      call ??
      vi.fn().mockResolvedValue({
        version: "0.1.0",
        output_device_substring: "",
        features: {
          midi_clock_in: false,
          midi_clock_out: false,
          ableton_link: false,
          sentry_telemetry: false,
          recording_enabled: true,
          rate_limit_disabled: false,
          shared_ci_runner: false,
        },
      } satisfies SessionInfo),
  }) as unknown as JsonRpcWS;

describe("AboutPanel", () => {
  beforeEach((): void => {
    __resetSessionInfo();
    __resetSyncStatus();
  });
  afterEach((): void => {
    cleanup();
  });

  it("renders loading placeholder before fetch resolves", () => {
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-version").textContent).toBe(
      "v(loading…)",
    );
  });

  it("renders version + device after store is seeded", () => {
    __setSessionInfo({
      version: "0.2.1",
      output_device_substring: "BlackHole 2ch",
      features: {
        midi_clock_in: true,
        midi_clock_out: false,
        ableton_link: false,
        sentry_telemetry: false,
        recording_enabled: true,
        rate_limit_disabled: false,
        shared_ci_runner: false,
      },
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-version").textContent).toBe("v0.2.1");
    expect(screen.getByTestId("about-output-device").textContent).toBe(
      "BlackHole 2ch",
    );
  });

  it("falls back to (system default) when output_device_substring is empty", () => {
    __setSessionInfo({
      version: "0.1.0",
      output_device_substring: "",
      features: {
        midi_clock_in: false,
        midi_clock_out: false,
        ableton_link: false,
        sentry_telemetry: false,
        recording_enabled: true,
        rate_limit_disabled: false,
        shared_ci_runner: false,
      },
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-output-device").textContent).toBe(
      "(system default)",
    );
  });

  it("kicks off engine.session_info on mount", async () => {
    const call = vi.fn().mockResolvedValue({
      version: "0.1.0",
      output_device_substring: "",
      features: {
        midi_clock_in: false,
        midi_clock_out: false,
        ableton_link: false,
        sentry_telemetry: false,
        recording_enabled: true,
        rate_limit_disabled: false,
        shared_ci_runner: false,
      },
    });
    render(<AboutPanel client={makeClient(call)} />);
    await waitFor(() => {
      expect(call).toHaveBeenCalledWith("engine.session_info");
    });
  });

  it("renders library track count + no pending suffix when zero", () => {
    __setSyncStatus({
      library_track_count: 42,
      pending_push_count: 0,
      last_pull_micros: 0,
      last_push_micros: 0,
      last_pull_fetched: 0,
      last_pull_applied: 0,
      last_push_pushed: 0,
      last_tick_error: "",
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-library-count").textContent).toBe(
      "42 tracks",
    );
  });

  it("renders pending sync count when greater than zero", () => {
    __setSyncStatus({
      library_track_count: 42,
      pending_push_count: 3,
      last_pull_micros: 0,
      last_push_micros: 0,
      last_pull_fetched: 0,
      last_pull_applied: 0,
      last_push_pushed: 0,
      last_tick_error: "",
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-library-count").textContent).toBe(
      "42 tracks · 3 pending sync",
    );
  });

  it("shows 'never' for last sync before first daemon tick", () => {
    __setSyncStatus({
      library_track_count: 5,
      pending_push_count: 0,
      last_pull_micros: 0,
      last_push_micros: 0,
      last_pull_fetched: 0,
      last_pull_applied: 0,
      last_push_pushed: 0,
      last_tick_error: "",
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-last-sync").textContent).toBe("never");
  });

  it("shows seconds-ago string for a recent pull", () => {
    const nowMs = Date.now();
    __setSyncStatus({
      library_track_count: 5,
      pending_push_count: 0,
      last_pull_micros: (nowMs - 12_000) * 1000,
      last_push_micros: 0,
      last_pull_fetched: 0,
      last_pull_applied: 0,
      last_push_pushed: 0,
      last_tick_error: "",
    });
    render(<AboutPanel client={makeClient()} />);
    const text = screen.getByTestId("about-last-sync").textContent ?? "";
    expect(text.endsWith("s ago")).toBe(true);
  });

  it("appends tick-error suffix when daemon reports a fault", () => {
    const nowMs = Date.now();
    __setSyncStatus({
      library_track_count: 5,
      pending_push_count: 0,
      last_pull_micros: (nowMs - 1_000) * 1000,
      last_push_micros: 0,
      last_pull_fetched: 0,
      last_pull_applied: 0,
      last_push_pushed: 0,
      last_tick_error: "supabase: HTTP 503",
    });
    render(<AboutPanel client={makeClient()} />);
    const text = screen.getByTestId("about-last-sync").textContent ?? "";
    expect(text.includes("supabase: HTTP 503")).toBe(true);
  });

  it("renders all 7 feature flags", () => {
    __setSessionInfo({
      version: "0.1.0",
      output_device_substring: "",
      features: {
        midi_clock_in: true,
        midi_clock_out: false,
        ableton_link: true,
        sentry_telemetry: false,
        recording_enabled: true,
        rate_limit_disabled: false,
        shared_ci_runner: false,
      },
    });
    render(<AboutPanel client={makeClient()} />);
    for (const key of [
      "midi_clock_in",
      "midi_clock_out",
      "ableton_link",
      "sentry_telemetry",
      "recording_enabled",
      "rate_limit_disabled",
      "shared_ci_runner",
    ]) {
      expect(screen.getByTestId(`about-flag-${key}`)).toBeTruthy();
    }
  });
});
