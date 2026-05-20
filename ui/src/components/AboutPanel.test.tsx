// AboutPanel.test.tsx — render + RPC fetch + flag highlighting.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { AboutPanel } from "./AboutPanel";
import {
  __resetConnectionState,
  __setConnectionState,
} from "../store/connection";
import {
  __resetSessionInfo,
  __resetStemsStatus,
  __resetSyncStatus,
  __setSessionInfo,
  __setStemsStatus,
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
    __resetStemsStatus();
    __resetConnectionState();
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
      next_sync_micros: 0,
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
      next_sync_micros: 0,
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
      next_sync_micros: 0,
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
      next_sync_micros: 0,
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
      next_sync_micros: 0,
    });
    render(<AboutPanel client={makeClient()} />);
    const text = screen.getByTestId("about-last-sync").textContent ?? "";
    expect(text.includes("supabase: HTTP 503")).toBe(true);
  });

  it("renders 'sync now' button by default and fires sync_now RPC on click", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "engine.session_info") {
        return {
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
        };
      }
      if (method === "library.sync_now") {
        return {
          pending_push_count: 0,
          library_track_count: 7,
          last_pull_micros: Date.now() * 1000,
          last_push_micros: Date.now() * 1000,
          last_pull_fetched: 1,
          last_pull_applied: 1,
          last_push_pushed: 0,
          last_tick_error: "",
      next_sync_micros: 0,
        };
      }
      return {
        pending_push_count: 0,
        library_track_count: 0,
        last_pull_micros: 0,
        last_push_micros: 0,
        last_pull_fetched: 0,
        last_pull_applied: 0,
        last_push_pushed: 0,
        last_tick_error: "",
      next_sync_micros: 0,
      };
    });
    render(<AboutPanel client={makeClient(call)} />);
    const btn = screen.getByTestId("about-sync-now");
    expect(btn.textContent).toBe("sync now");
    fireEvent.click(btn);
    await waitFor(() => {
      expect(call).toHaveBeenCalledWith("library.sync_now");
    });
    await waitFor(() => {
      expect(screen.getByTestId("about-library-count").textContent).toBe(
        "7 tracks",
      );
    });
  });

  it("surfaces sync_now errors inline", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "engine.session_info") {
        return {
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
        };
      }
      if (method === "library.sync_now") {
        throw new Error("cloud sync not configured");
      }
      return {
        pending_push_count: 0,
        library_track_count: 0,
        last_pull_micros: 0,
        last_push_micros: 0,
        last_pull_fetched: 0,
        last_pull_applied: 0,
        last_push_pushed: 0,
        last_tick_error: "",
      next_sync_micros: 0,
      };
    });
    render(<AboutPanel client={makeClient(call)} />);
    fireEvent.click(screen.getByTestId("about-sync-now"));
    await waitFor(() => {
      expect(screen.getByTestId("about-sync-error").textContent).toBe(
        "cloud sync not configured",
      );
    });
  });

  it("renders 'next in Xs' countdown when next_sync_micros set", () => {
    const nowMs = Date.now();
    __setSyncStatus({
      library_track_count: 5,
      pending_push_count: 0,
      last_pull_micros: (nowMs - 3_000) * 1000,
      last_push_micros: 0,
      last_pull_fetched: 0,
      last_pull_applied: 0,
      last_push_pushed: 0,
      last_tick_error: "",
      next_sync_micros: (nowMs + 45_000) * 1000,
    });
    render(<AboutPanel client={makeClient()} />);
    const text = screen.getByTestId("about-last-sync").textContent ?? "";
    expect(text.includes("next in ")).toBe(true);
    // Expect roughly "45s" — widen to absorb slow-CI jitter
    // (Codex #174 R1 review note).
    expect(/next in (4[0-9]|5[0-2])s/.test(text)).toBe(true);
  });

  it("renders 'queue all' button + toast on success", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "engine.session_info") {
        return {
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
        };
      }
      if (method === "library.requeue_all_pending") {
        return { queued: 42 };
      }
      return {
        pending_push_count: 0,
        library_track_count: 0,
        last_pull_micros: 0,
        last_push_micros: 0,
        last_pull_fetched: 0,
        last_pull_applied: 0,
        last_push_pushed: 0,
        last_tick_error: "",
        next_sync_micros: 0,
      };
    });
    render(<AboutPanel client={makeClient(call)} />);
    const btn = screen.getByTestId("about-queue-all");
    expect(btn.textContent).toBe("queue all");
    fireEvent.click(btn);
    await waitFor(() => {
      expect(call).toHaveBeenCalledWith("library.requeue_all_pending");
    });
    await waitFor(() => {
      expect(
        screen.getByTestId("about-queue-all-toast").textContent,
      ).toBe("42 queued for sync");
    });
  });

  it("queue-all toast auto-dismisses after 4s", async () => {
    vi.useFakeTimers();
    const call = vi.fn(async (method: string) => {
      if (method === "engine.session_info") {
        return {
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
        };
      }
      if (method === "library.requeue_all_pending") {
        return { queued: 7 };
      }
      return {
        pending_push_count: 0,
        library_track_count: 0,
        last_pull_micros: 0,
        last_push_micros: 0,
        last_pull_fetched: 0,
        last_pull_applied: 0,
        last_push_pushed: 0,
        last_tick_error: "",
        next_sync_micros: 0,
      };
    });
    render(<AboutPanel client={makeClient(call)} />);
    fireEvent.click(screen.getByTestId("about-queue-all"));
    // Wait for the toast to appear (real microtask + state update).
    await vi.waitFor(() => {
      expect(screen.getByTestId("about-queue-all-toast").textContent).toBe(
        "7 queued for sync",
      );
    });
    // Advance fake clock past the auto-dismiss window.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(4100);
    });
    expect(screen.queryByTestId("about-queue-all-toast")).toBeNull();
    vi.useRealTimers();
  });

  it("queue-all surfaces errors via the shared sync-error region", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "engine.session_info") {
        return {
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
        };
      }
      if (method === "library.requeue_all_pending") {
        throw new Error("cloud sync not configured");
      }
      return {
        pending_push_count: 0,
        library_track_count: 0,
        last_pull_micros: 0,
        last_push_micros: 0,
        last_pull_fetched: 0,
        last_pull_applied: 0,
        last_push_pushed: 0,
        last_tick_error: "",
        next_sync_micros: 0,
      };
    });
    render(<AboutPanel client={makeClient(call)} />);
    fireEvent.click(screen.getByTestId("about-queue-all"));
    await waitFor(() => {
      expect(screen.getByTestId("about-sync-error").textContent).toBe(
        "cloud sync not configured",
      );
    });
  });

  it("hides sync counts row when all tick counters are zero", () => {
    __setSyncStatus({
      library_track_count: 5,
      pending_push_count: 0,
      last_pull_micros: Date.now() * 1000,
      last_push_micros: 0,
      last_pull_fetched: 0,
      last_pull_applied: 0,
      last_push_pushed: 0,
      last_tick_error: "",
      next_sync_micros: 0,
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.queryByTestId("about-sync-counts")).toBeNull();
  });

  it("shows fetched-only count when applied=0 and pushed=0", () => {
    __setSyncStatus({
      library_track_count: 5,
      pending_push_count: 0,
      last_pull_micros: Date.now() * 1000,
      last_push_micros: 0,
      last_pull_fetched: 3,
      last_pull_applied: 0,
      last_push_pushed: 0,
      last_tick_error: "",
      next_sync_micros: 0,
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-sync-counts").textContent).toBe(
      "↓ 3 fetched",
    );
  });

  it("shows fetched + applied + pushed when all non-zero", () => {
    __setSyncStatus({
      library_track_count: 5,
      pending_push_count: 0,
      last_pull_micros: Date.now() * 1000,
      last_push_micros: Date.now() * 1000,
      last_pull_fetched: 7,
      last_pull_applied: 5,
      last_push_pushed: 2,
      last_tick_error: "",
      next_sync_micros: 0,
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-sync-counts").textContent).toBe(
      "↓ 7 fetched · 5 applied · ↑ 2 pushed",
    );
  });

  it("shows push-only when pushed>0 and pull counts=0", () => {
    __setSyncStatus({
      library_track_count: 5,
      pending_push_count: 0,
      last_pull_micros: 0,
      last_push_micros: Date.now() * 1000,
      last_pull_fetched: 0,
      last_pull_applied: 0,
      last_push_pushed: 4,
      last_tick_error: "",
      next_sync_micros: 0,
    });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-sync-counts").textContent).toBe(
      "↓ 0 fetched · ↑ 4 pushed",
    );
  });

  it("hides the Stems row when ready+pending+failed are all zero", () => {
    __setStemsStatus({ ready: 0, pending: 0, failed: 0, none: 12 });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.queryByTestId("about-stems-status")).toBeNull();
  });

  it("renders Stems row with ready-only count when no pending/failed", () => {
    __setStemsStatus({ ready: 3, pending: 0, failed: 0, none: 9 });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-stems-status").textContent).toBe(
      "3 ready",
    );
  });

  it("renders Stems row with all three counts when populated", () => {
    __setStemsStatus({ ready: 5, pending: 2, failed: 1, none: 4 });
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-stems-status").textContent).toBe(
      "5 ready · 2 pending · 1 failed",
    );
  });

  it("renders 'offline' badge when WS connection is closed", () => {
    __setConnectionState("closed");
    render(<AboutPanel client={makeClient()} />);
    const badge = screen.getByTestId("about-engine-offline");
    expect(badge.textContent).toBe("offline");
  });

  it("renders 'reconnect' button alongside offline badge", () => {
    __setConnectionState("closed");
    render(<AboutPanel client={makeClient()} />);
    expect(screen.getByTestId("about-reconnect-now")).toBeTruthy();
  });

  it("clicking 'reconnect' calls client.reconnectNow() if exposed", () => {
    __setConnectionState("closed");
    const reconnectNow = vi.fn();
    const client = {
      call: vi.fn().mockResolvedValue({}),
      reconnectNow,
    } as unknown as JsonRpcWS;
    render(<AboutPanel client={client} />);
    fireEvent.click(screen.getByTestId("about-reconnect-now"));
    expect(reconnectNow).toHaveBeenCalledTimes(1);
  });

  it("clicking 'reconnect' is a no-op when client lacks reconnectNow", () => {
    __setConnectionState("closed");
    render(<AboutPanel client={makeClient()} />);
    expect((): void => {
      fireEvent.click(screen.getByTestId("about-reconnect-now"));
    }).not.toThrow();
  });

  it("hides 'offline' badge when WS connection is open", () => {
    __setConnectionState("open");
    render(<AboutPanel client={makeClient()} />);
    expect(screen.queryByTestId("about-engine-offline")).toBeNull();
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
