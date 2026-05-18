// Tests for the opt-in Sentry telemetry hook.
//
// We avoid pulling in the real `@sentry/react` SDK by mocking the
// module at the top of the file. This keeps the test suite hermetic —
// CI does not need network access to validate the wiring.

import { afterEach, describe, expect, it, vi } from "vitest";

// `vi.hoisted` lets us declare a mock spy that the hoisted `vi.mock`
// factory below is allowed to reference. Without `hoisted`, the
// factory runs before the spy is defined and we get a ReferenceError.
const { sentryInit } = vi.hoisted(() => ({ sentryInit: vi.fn() }));
vi.mock("@sentry/react", () => ({
  init: sentryInit,
  default: { init: sentryInit },
}));

import {
  initTelemetry,
  resolveEnabled,
  scrubPii,
  scrubString,
} from "./telemetry";

afterEach(() => {
  sentryInit.mockReset();
});

describe("resolveEnabled", () => {
  it("returns 'off' when nothing is set", () => {
    expect(resolveEnabled(undefined, {})).toBe("off");
    expect(resolveEnabled({}, {})).toBe("off");
  });

  it("returns 'window' when the window flag is true", () => {
    expect(
      resolveEnabled({ __HYPEHOUSE_TELEMETRY_ENABLED__: true }, {}),
    ).toBe("window");
  });

  it("falls through to env when the window flag is missing", () => {
    expect(
      resolveEnabled({}, { VITE_TELEMETRY_ENABLED: "1" }),
    ).toBe("env");
    expect(
      resolveEnabled({}, { VITE_TELEMETRY_ENABLED: "true" }),
    ).toBe("env");
    expect(
      resolveEnabled({}, { VITE_TELEMETRY_ENABLED: "yes" }),
    ).toBe("env");
  });

  it("treats falsy env values as off", () => {
    expect(
      resolveEnabled({}, { VITE_TELEMETRY_ENABLED: "0" }),
    ).toBe("off");
    expect(
      resolveEnabled({}, { VITE_TELEMETRY_ENABLED: "false" }),
    ).toBe("off");
    expect(
      resolveEnabled({}, { VITE_TELEMETRY_ENABLED: "" }),
    ).toBe("off");
  });
});

describe("scrubString", () => {
  it("collapses home-directory paths", () => {
    expect(scrubString("/Users/jane/Music/track.mp3")).toBe("<scrubbed-path>");
    expect(scrubString("/home/jane/Music/track.mp3")).toBe("<scrubbed-path>");
    expect(scrubString("C:\\Users\\jane\\track.mp3")).toBe("<scrubbed-path>");
  });

  it("keeps only the basename for other paths", () => {
    expect(scrubString("/tmp/cache/file.dat")).toBe("file.dat");
    expect(scrubString("relative/file.dat")).toBe("file.dat");
  });

  it("strips query strings from URLs", () => {
    expect(scrubString("https://cdn.example.com/track.mp3?sig=abc")).toBe(
      "https://cdn.example.com/track.mp3",
    );
  });

  it("passes through non-path strings", () => {
    expect(scrubString("hello")).toBe("hello");
    expect(scrubString("decode_panic")).toBe("decode_panic");
  });
});

describe("scrubPii", () => {
  it("drops headers, cookies, user, server_name", () => {
    const event = {
      request: {
        headers: { Authorization: "Bearer secret" },
        cookies: "sess=abc",
        url: "https://app/x",
      },
      user: { username: "jane" },
      server_name: "jane-mbp",
      extra: { track_path: "/Users/jane/Music/x.mp3", ok: "leave-me" },
      breadcrumbs: {
        values: [
          { message: "/Users/jane/Music/y.mp3", data: { path: "/Users/jane/z" } },
        ],
      },
    };
    const out = scrubPii(event) as typeof event;
    expect(out.request.headers).toBeUndefined();
    expect(out.request.cookies).toBeUndefined();
    expect(out.user).toBeUndefined();
    expect(out.server_name).toBeUndefined();
    expect(out.extra.track_path).toBe("<scrubbed-path>");
    expect(out.extra.ok).toBe("leave-me");
    expect(out.breadcrumbs.values[0].message).toBe("<scrubbed-path>");
    expect((out.breadcrumbs.values[0].data as { path: string }).path).toBe(
      "<scrubbed-path>",
    );
  });

  it("is a no-op on non-objects", () => {
    expect(scrubPii(null)).toBeNull();
    expect(scrubPii("string")).toBe("string");
  });
});

describe("initTelemetry", () => {
  it("does not call Sentry.init when the opt-in flags are missing", () => {
    const result = initTelemetry({ window: {}, env: {}, sentry: { init: sentryInit } });
    expect(result).toBe(false);
    expect(sentryInit).not.toHaveBeenCalled();
  });

  it("calls Sentry.init when the window flag is set", () => {
    const result = initTelemetry({
      window: { __HYPEHOUSE_TELEMETRY_ENABLED__: true },
      env: { VITE_TELEMETRY_DSN: "https://k@o0.ingest.sentry.io/1" },
      sentry: { init: sentryInit },
    });
    expect(result).toBe(true);
    expect(sentryInit).toHaveBeenCalledTimes(1);
    const arg = sentryInit.mock.calls[0]?.[0] as {
      dsn: string;
      tracesSampleRate: number;
      beforeSend: (e: unknown) => unknown;
    };
    expect(arg.dsn).toBe("https://k@o0.ingest.sentry.io/1");
    expect(arg.tracesSampleRate).toBe(0.1);
    // beforeSend should be the scrubber — feed it a request with
    // headers to check the wiring.
    const scrubbed = arg.beforeSend({ request: { headers: { x: "y" } } }) as {
      request: { headers?: unknown };
    };
    expect(scrubbed.request.headers).toBeUndefined();
  });

  it("calls Sentry.init via env flag too", () => {
    const result = initTelemetry({
      window: {},
      env: { VITE_TELEMETRY_ENABLED: "1" },
      sentry: { init: sentryInit },
    });
    expect(result).toBe(true);
    expect(sentryInit).toHaveBeenCalledTimes(1);
  });
});
