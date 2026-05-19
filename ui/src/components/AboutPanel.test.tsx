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
  __setSessionInfo,
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
