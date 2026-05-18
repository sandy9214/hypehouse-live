# Telemetry (opt-in Sentry crash + perf monitoring)

**Status**: v0.1
**Owner**: engine + copilot + UI
**Default**: **OFF.** No data leaves the machine until the operator explicitly turns this on.

HypeHouse Live ships an optional Sentry hook in all three runtime
components — the Rust audio engine, the Python copilot, and the
TypeScript UI. When enabled, panics, unhandled exceptions, and (for the
UI) a 10 % sample of performance traces are forwarded to a Sentry
project so the team can debug crashes that the user could not reproduce
on demand.

Telemetry is **disabled by default** and stays disabled unless **you**
flip the switch.

---

## Why opt-in?

A live DJ rig touches files (your music library) and audio output. A
crash report could plausibly leak filenames, library paths, or even
in-progress track URLs if the integration were naive. Rather than ship
"on by default with PII scrubbing" — which is the industry norm but
still leaves a footgun for the contributor who adds the next
`tracing::error!("loading {path}")` — we ship "off by default, opt in
with one env var".

Our privacy commitments when telemetry **is** enabled:

* **No `user.*` fields are populated.** Usernames, emails, IP
  addresses are never attached to events.
* **Request headers + cookies are dropped.** The WS bridge's bearer
  token cannot leak via a panic context.
* **Filesystem paths are scrubbed.** Strings starting with
  `/Users/...`, `/home/...`, or `C:\Users\...` are collapsed to
  `<scrubbed-path>`. Other paths keep only their basename.
* **Track URLs are stripped of query strings** — signed CDN URLs do
  not carry their auth tokens to Sentry.
* **No track-name strings are attached deliberately.** A future
  contributor who adds one without thinking will see it scrubbed by
  the `before_send` heuristics anyway.

The DSN is a placeholder. You replace it with your own before
turning telemetry on — see *Setup* below.

---

## How to enable

### Engine (Rust)

```bash
HYPEHOUSE_TELEMETRY_ENABLED=1 \
HYPEHOUSE_TELEMETRY_DSN="https://<key>@<your-sentry>.ingest.sentry.io/<project>" \
./hypehouse-engine
```

Or persist via the config file at
`~/.config/hypehouse-live/telemetry.toml`:

```toml
enabled = true
```

(The env var wins if both are present.)

### Copilot (Python)

Install the `telemetry` extra:

```bash
pip install 'hypehouse-copilot[telemetry]'
```

then run with the same env vars:

```bash
HYPEHOUSE_TELEMETRY_ENABLED=1 \
HYPEHOUSE_TELEMETRY_DSN="..." \
python -m copilot
```

If `sentry-sdk` is not installed, the copilot logs a warning and
keeps running with telemetry off — it never crashes on a missing
extra.

### UI (TypeScript)

Set either of the following before `vite build`:

* `VITE_TELEMETRY_ENABLED=1` (build-time)
* `VITE_TELEMETRY_DSN=https://...`
* or, at runtime, set `window.__HYPEHOUSE_TELEMETRY_ENABLED__ = true`
  before the bundle loads (useful for in-host Tauri injection).

---

## Setup: self-hosting Sentry

The placeholder DSN
(`https://examplePublicKey@o4500000.ingest.sentry.io/4500000000000000`)
will not work — it is intentionally bogus. Your options:

1. **Self-host Sentry.** Follow the
   [Sentry self-hosted install](https://develop.sentry.dev/self-hosted/)
   docs. Once running, copy the project DSN into
   `HYPEHOUSE_TELEMETRY_DSN`.
2. **Use Sentry SaaS.** Create a free Sentry.io project, copy the DSN.
3. **Replace the placeholder in source.** Fork operators editing
   `engine/src/telemetry.rs`, `copilot/telemetry.py`, and
   `ui/src/telemetry.ts` can bake a DSN in for their internal
   distribution.

---

## What gets scrubbed (in detail)

| Source              | Scrubbed                                                    | Kept                                    |
|---------------------|-------------------------------------------------------------|-----------------------------------------|
| Request headers     | All (Authorization, Cookie, X-*)                            | —                                       |
| Request cookies     | All                                                         | —                                       |
| Request query strings | All                                                       | —                                       |
| User object         | username, email, ip_address, id                             | —                                       |
| Server hostname     | (dropped)                                                   | —                                       |
| `extra.*` strings   | Filesystem paths collapsed; home-dir paths → `<scrubbed-path>` | Non-path strings                       |
| `tags.*` strings    | Same                                                        | Same                                    |
| Breadcrumb messages | Same                                                        | Same                                    |
| URL query strings   | Stripped                                                    | scheme + host + path                    |
| Stack traces        | (Sentry default redaction)                                  | Function names, crate names, line numbers |

The scrubbers live in:

* `engine/src/telemetry.rs::scrub_pii` (Rust)
* `copilot/telemetry.py::scrub_pii` (Python)
* `ui/src/telemetry.ts::scrubPii` (TypeScript)

Each is unit-tested with adversarial inputs (`/Users/jane/...`,
nested objects, breadcrumb arrays) — see `*.test.*` siblings to the
scrubber files.

---

## How to verify telemetry is off

```bash
# Engine
unset HYPEHOUSE_TELEMETRY_ENABLED
./hypehouse-engine  # logs: "telemetry: disabled (set ...)"

# Copilot
unset HYPEHOUSE_TELEMETRY_ENABLED
python -m copilot   # logs: "telemetry: disabled (set ...)"

# UI
# Open devtools, console; see: "telemetry: disabled ..."
```

If you see `"telemetry: enabled ..."` in any of those logs, you have
opted in. The guard / SDK is initialised; events will be sent on the
next panic / exception / route change.

---

## Disabling once enabled

* **Engine + copilot**: unset `HYPEHOUSE_TELEMETRY_ENABLED` and
  delete (or set `enabled = false` in) the config file.
* **UI**: rebuild without `VITE_TELEMETRY_ENABLED`, or set the window
  flag to `false` before bundle load.
* No persistent state survives — telemetry runs in-process only.

---

## Compliance

We do not collect telemetry without opt-in. If you find a code path
that ships an event when neither switch is set, that is a bug —
please open an issue with the `area:privacy` and `priority:p1`
labels.
