// Opt-in Sentry telemetry for the UI.
//
// Privacy contract (same as engine + copilot):
//   * OFF by default.
//   * Enabled when `window.__HYPEHOUSE_TELEMETRY_ENABLED__ === true`
//     OR when `import.meta.env.VITE_TELEMETRY_ENABLED === "1" | "true"`.
//   * Every event passes through `scrubPii` before send. We drop
//     request headers, scrub track URLs / file paths, and strip the
//     `user` object.
//   * DSN comes from `import.meta.env.VITE_TELEMETRY_DSN` if set,
//     else the hardcoded `PLACEHOLDER_DSN` below — fork operators
//     replace before shipping.
//
// `initTelemetry()` is wired in `main.tsx` BEFORE the React render
// so unhandled errors during boot are captured.

import * as Sentry from "@sentry/react";

export const PLACEHOLDER_DSN =
  "https://examplePublicKey@o4500000.ingest.sentry.io/4500000000000000";

type SentryLike = Pick<typeof Sentry, "init">;

interface TelemetryWindow {
  __HYPEHOUSE_TELEMETRY_ENABLED__?: boolean;
}

interface ImportMetaEnvShape {
  readonly VITE_TELEMETRY_ENABLED?: string;
  readonly VITE_TELEMETRY_DSN?: string;
  readonly VITE_TELEMETRY_ENVIRONMENT?: string;
  readonly MODE?: string;
}

const HOME_PATH_PREFIXES = [
  "/Users/",
  "/home/",
  "C:\\Users\\",
  "C:/Users/",
];

const isTruthy = (v: string | undefined | null): boolean => {
  if (!v) return false;
  const lower = v.trim().toLowerCase();
  return lower === "1" || lower === "true" || lower === "yes" || lower === "on";
};

const readEnv = (): ImportMetaEnvShape => {
  const meta = import.meta as unknown as { env?: ImportMetaEnvShape };
  return meta.env ?? {};
};

export const resolveEnabled = (
  win: TelemetryWindow | undefined,
  env: ImportMetaEnvShape,
): "window" | "env" | "off" => {
  if (win?.__HYPEHOUSE_TELEMETRY_ENABLED__ === true) return "window";
  if (isTruthy(env.VITE_TELEMETRY_ENABLED)) return "env";
  return "off";
};

export const scrubString = (s: string): string => {
  for (const p of HOME_PATH_PREFIXES) {
    if (s.startsWith(p)) return "<scrubbed-path>";
  }
  // Strip query strings on URL-shaped values — track URLs may carry
  // signed auth tokens. We keep the path so panic stack frames stay
  // useful.
  if (/^https?:\/\//i.test(s)) {
    try {
      const u = new URL(s);
      return `${u.protocol}//${u.host}${u.pathname}`;
    } catch {
      return s;
    }
  }
  if (s.includes("/") || s.includes("\\")) {
    const parts = s.split(/[\\/]/);
    const tail = parts[parts.length - 1];
    return tail || "<scrubbed-path>";
  }
  return s;
};

const scrubAny = (v: unknown): unknown => {
  if (typeof v === "string") return scrubString(v);
  if (Array.isArray(v)) return v.map(scrubAny);
  if (v && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
      out[k] = scrubAny(val);
    }
    return out;
  }
  return v;
};

type AnyEvent = Record<string, unknown> & {
  request?: Record<string, unknown>;
  user?: unknown;
  server_name?: unknown;
  extra?: Record<string, unknown>;
  tags?: Record<string, unknown>;
  contexts?: Record<string, unknown>;
  breadcrumbs?: Array<Record<string, unknown>> | { values?: Array<Record<string, unknown>> };
};

export const scrubPii = (eventIn: unknown): unknown => {
  if (!eventIn || typeof eventIn !== "object") return eventIn;
  const event = eventIn as AnyEvent;
  if (event.request && typeof event.request === "object") {
    delete event.request.headers;
    delete event.request.cookies;
    delete event.request.query_string;
  }
  delete event.user;
  delete event.server_name;
  for (const key of ["extra", "tags", "contexts"] as const) {
    if (event[key]) event[key] = scrubAny(event[key]) as Record<string, unknown>;
  }
  const bc = event.breadcrumbs;
  const list = Array.isArray(bc) ? bc : bc?.values;
  if (Array.isArray(list)) {
    for (const b of list) {
      if (typeof b.message === "string") b.message = scrubString(b.message);
      if (b.data) b.data = scrubAny(b.data);
    }
  }
  return event;
};

export interface InitTelemetryDeps {
  readonly window?: TelemetryWindow;
  readonly env?: ImportMetaEnvShape;
  readonly sentry?: SentryLike;
}

/**
 * Initialise Sentry if the user has opted in. Returns `true` when the
 * SDK was actually initialised, `false` otherwise. The function is
 * idempotent only at the SDK level — calling it twice will re-invoke
 * `Sentry.init`, which is harmless in practice but pointless.
 *
 * `deps` is exposed for unit tests; production callers pass nothing.
 */
export const initTelemetry = (deps: InitTelemetryDeps = {}): boolean => {
  const w = deps.window ?? (typeof window !== "undefined" ? (window as TelemetryWindow) : undefined);
  const env = deps.env ?? readEnv();
  const sentry = deps.sentry ?? Sentry;

  const decision = resolveEnabled(w, env);
  if (decision === "off") {
    // eslint-disable-next-line no-console
    console.info(
      "telemetry: disabled (set window.__HYPEHOUSE_TELEMETRY_ENABLED__ or VITE_TELEMETRY_ENABLED to opt in)",
    );
    return false;
  }

  const dsn = (env.VITE_TELEMETRY_DSN ?? PLACEHOLDER_DSN).trim();
  if (!dsn) {
    // eslint-disable-next-line no-console
    console.info("telemetry: DSN empty — staying disabled");
    return false;
  }
  const environment =
    (env.VITE_TELEMETRY_ENVIRONMENT ?? env.MODE ?? "production").trim() ||
    "production";
  try {
    sentry.init({
      dsn,
      environment,
      release: `hypehouse-ui@0.1.0`,
      tracesSampleRate: 0.1,
      sendDefaultPii: false,
      beforeSend: (event: unknown): unknown => scrubPii(event),
    } as unknown as Parameters<SentryLike["init"]>[0]);
  } catch (e) {
    // eslint-disable-next-line no-console
    console.warn("telemetry: Sentry.init failed", e);
    return false;
  }
  // eslint-disable-next-line no-console
  console.info(`telemetry: enabled via ${decision} — Sentry SDK initialised`);
  return true;
};
