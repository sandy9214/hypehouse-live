# WebSocket bridge — JSON-RPC 2.0 protocol

**Status**: v0.1 (engine ↔ UI ↔ copilot)
**Owner**: engine crate, module `bridge`
**Source of truth**: `engine/src/bridge/rpc.rs` (method dispatch),
`engine/src/state.rs` (`Event` / `EventKind` / `EngineState` types).

<!--
SOURCE-OF-TRUTH NOTE — read before editing this doc.

`engine/src/state.rs` is THE source of truth for event shapes (variant
names + field names + types). This document mirrors it so external
clients (UI, copilot, third-party MIDI tools, browser-mode auth
testers) have something to read without grepping Rust.

If you find drift between this doc and `state.rs`, **the Rust file
wins** — update the doc, do not change the engine to match.

Issue #27 caught one round of drift: an early spec brief said
`PitchAdjust { value }`, `EqAdjust { value }`, `HotCue* { index }`.
The engine actually ships `PitchBend { semitones }`,
`EqAdjust { value_db }`, `HotCue* { slot }`. All surfaces (UI + copilot
+ this doc) now agree with the engine.
-->

Related ADRs: [ADR-001 stack choice](../adr/ADR-001-stack-choice.md),
[ADR-003 event-sourced state](../adr/ADR-003-event-sourced-state.md).

## Transport

* Protocol: WebSocket (RFC 6455).
* Default address: `ws://127.0.0.1:8765`. Override port via
  `HYPEHOUSE_WS_PORT`; override the full bind addr via
  `HYPEHOUSE_WS_BIND_ADDR=<ip>:<port>`.
* Framing: every text frame is a single JSON value. Binary frames are
  rejected with `-32600`.
* Encoding: UTF-8 JSON.

## Auth

Two modes, both produce the same authenticated steady-state.

### Header auth (native clients — Tauri / Rust integration)

| `HYPEHOUSE_BRIDGE_TOKEN` | Bind addr policy           | Per-connection check                     |
|--------------------------|----------------------------|------------------------------------------|
| **unset / empty**        | Forced to `127.0.0.1`      | None — every handshake accepted.         |
| **set to `<token>`**     | Caller's choice (default loopback) | `Authorization: Bearer <token>` required on the WS upgrade. Header **present and wrong** → handshake fails with HTTP 401. Header **absent** → upgrade accepted in `PendingAuth` state (see browser mode below). |

Rationale: the unauthenticated mode literally cannot accept a remote
connection, so a forgotten token never widens the attack surface.
Explicit-but-wrong tokens still fail fast at the upgrade for native
clients (no in-band retries needed).

### Browser-mode auth (in-band `auth.hello`)

Browsers cannot attach custom headers to a WebSocket upgrade, so the
engine accepts header-less connections in a **pending-auth** state. The
client must call `auth.hello` as the **first JSON-RPC method** on the
socket. Every other method short-circuits with `-32002 AUTH_REJECTED`
until the handshake completes.

Request:

```json
{
  "jsonrpc": "2.0",
  "method": "auth.hello",
  "params": { "token": "<bearer>" },
  "id": 1
}
```

Success response — the connection transitions to authed:

```json
{
  "jsonrpc": "2.0",
  "result": { "authed": true, "session": 1734567890123456 },
  "id": 1
}
```

`session` is a micros-since-UNIX-epoch marker the client can correlate
with engine logs. It is **not** a credential; the bearer token still
gates the connection.

Failure response — invalid token, connection stays in `PendingAuth` and
the client may retry within the timeout window:

```json
{
  "jsonrpc": "2.0",
  "error": { "code": -32002, "message": "Authentication rejected", "data": "invalid token" },
  "id": 1
}
```

Idempotency: once a connection is authed, replaying `auth.hello` with
the valid token is a no-op success (still requires the correct token).
The handshake never regresses the state machine.

**Pending-auth idle timeout**: a connection that does not send a
successful `auth.hello` within **5 seconds** is closed by the server
with WebSocket close code **1008 ("policy violation")**, reason
`"auth.hello timeout"`. Browser clients SHOULD send `auth.hello`
immediately after `onopen`; queueing it behind UI bootstrapping risks
the eviction.

When `HYPEHOUSE_BRIDGE_TOKEN` is unset the gate is a no-op: every
connection is admitted as `Authed` from the first frame and `auth.hello`
returns success against any token (the server simply has no token to
compare).

## Method catalog

The auth method is namespaced `auth.*`; everything else is `engine.*`.
Request envelope:

```json
{ "jsonrpc": "2.0", "method": "<name>", "params": <object>, "id": <num|str> }
```

Response envelope on success:

```json
{ "jsonrpc": "2.0", "result": <value>, "id": <num|str> }
```

### `auth.hello`

In-band bearer-token handshake for browser WS clients. See the
**Browser-mode auth** section above for the full state-machine
contract, idempotency, and idle-timeout policy.

**Params**
```json
{ "token": "<bearer>" }
```

**Result**
```json
{ "authed": true, "session": 1734567890123456 }
```

**Errors**: `-32002 AUTH_REJECTED` on invalid token; `-32602
INVALID_PARAMS` if the params shape is missing the `token` field.

### `engine.submit_event`

Append an event to the log, run the reducer, and fan the new state out
to every connected client as an `engine.state_changed` notification.

**Params**

Either wrapped:
```json
{ "kind": { "DeckPlay": { "deck": "A" } }, "source": { "Ui": null } }
```

Or bare (server defaults `source` to `Ui`):
```json
{ "DeckPlay": { "deck": "A" } }
```

The `kind` shape is the serde-tagged enum of `state::EventKind`. The
server stamps `id` (monotonic) and `ts_micros` (engine clock) so the
client doesn't have to.

#### Event catalog (authoritative)

Mirrors `EventKind` in `engine/src/state.rs`. Field names + types are
exactly what the serde-derived JSON wire format uses; this table is
generated by hand and verified against `state.rs` on every change.
**If this table disagrees with `state.rs`, the Rust file wins.**

| Variant             | Payload (field: type)                                                          | Notes                                              |
|---------------------|--------------------------------------------------------------------------------|----------------------------------------------------|
| `SessionStart`      | (none)                                                                         | Marks `session_active = true`.                     |
| `SessionEnd`        | (none)                                                                         | Marks `session_active = false`.                    |
| `DeckLoad`          | `deck: "A"\|"B"`, `track: TrackRef`, `bpm: f32`, `beat_grid_anchor_ms: u64`, `downbeats_ms?: u32[]`, `hot_cues?: (u64 \| null)[8]` | Full payload example below. `hot_cues` carries the library's saved cue grid.       |
| `DeckLoadStems`     | `deck`, `track: TrackRef`, `stem_paths: [string; 4]`, `bpm: f32`, `beat_grid_anchor_ms: u64`, `downbeats_ms?: u32[]`, `hot_cues?: (u64 \| null)[8]` | Stem-aware load — `stem_paths` ordered `[vocals, drums, bass, other]` (e.g. from `library.get_stems`). Mutually exclusive with `DeckLoad` on the same deck. Audio thread pulls 4 independent decode streams + MACs with `Deck.stem_gains`. |
| `SetStemGain`       | `deck`, `stem: u8`, `gain: f32`                                                | Per-stem linear gain in `[0, 1]`. `stem ∈ 0..4` (0=vocals, 1=drums, 2=bass, 3=other); out-of-range silently ignored. Only active when the deck is in stem-mode. |
| `DeckUnload`        | `deck`                                                                         | Clears deck state to default.                      |
| `DeckPlay`          | `deck`                                                                         |                                                    |
| `DeckPause`         | `deck`                                                                         |                                                    |
| `DeckCue`           | `deck`, `position_ms: u64`                                                     | Seek to position.                                  |
| `Crossfader`        | `value: f32`                                                                   | 0.0 = full A, 1.0 = full B. Clamped to [0, 1].     |
| `SetCrossfaderCurve`| `curve: "Linear" \| "Dipped" \| "Sharp" \| "Scratch"`                          | Crossfader response curve (`engine/src/state.rs::CrossfaderCurve`). Default `Linear`. Curve dispatch is per-block on the audio thread; switching is alloc-free + glitch-free (≤ one buffer of latency). See `engine/src/audio/mixer.rs` module docs for the math of each variant. |
| `EqAdjust`          | `deck`, `band: "Low"\|"Mid"\|"High"`, `value_db: f32`                          | Clamped to `[-26.0, 6.0]` dB (pro convention).     |
| `HotCueSet`         | `deck`, `slot: u8`, `position_ms: u64`                                         | `slot ∈ 0..8`; out-of-range slots ignored.         |
| `HotCueTrigger`     | `deck`, `slot: u8`                                                             | Seeks to the saved position if `slot` exists.      |
| `LoopIn`            | `deck`                                                                         | Stamps loop-in at current `position_ms`.           |
| `LoopOut`           | `deck`                                                                         | Stamps loop-out; arms loop only if `LoopIn` was set. |
| `LoopExit`          | `deck`                                                                         | Clears loop in/out + deactivates.                  |
| `PitchBend`         | `deck`, `semitones: f32`                                                       | **Pure pitch shift** (independent of tempo). Clamped to `[-12.0, 12.0]`. **Not `PitchAdjust`.** |
| `TempoBend`         | `deck`, `ratio: f32`                                                           | **Pure tempo shift** (independent of pitch). 1.0 = normal speed. Clamped to `[0.5, 2.0]`; non-finite → 1.0. |
| `PitchTempoReset`   | `deck`                                                                         | Reset both `pitch_semitones` to 0 and `tempo_ratio` to 1.0 on the deck. |
| `PhaseNudge`        | `deck`, `delta_ms: i32`                                                        | Accumulates onto `phase_offset_ms`. ADR-007.       |
| `EffectAssign`      | `deck`, `slot: u8`, `effect_id: u32`                                           | `slot ∈ 0..3`. `effect_id` from `engine.list_effects`. |
| `EffectClear`       | `deck`, `slot: u8`                                                             | Resets the slot to empty.                          |
| `EffectParam`       | `deck`, `slot: u8`, `param: string`, `value: f32`                              | Param name from the effect's manifest.             |
| `EffectWetDry`      | `deck`, `slot: u8`, `value: f32`                                               | Clamped to `[0, 1]`.                               |
| `EffectEnable`      | `deck`, `slot: u8`, `enabled: bool`                                            | Also clears any in-flight `one_shot` on the slot — explicit toggle supersedes scheduled disengage. |
| `EffectOneShot`     | `deck`, `slot: u8`, `beats: u8`                                                | "Beat-FX one-shot" — momentary engage. Engine forces `enabled = true`, stores `(was_enabled, ends_at_micros)` on the slot's `one_shot` field. `beats` clamped `1..=64` (0 → 1). When the deck has no beat grid (`beat_period_ms = 0`), falls back to 500 ms per beat. Auto-disengage scheduling (audio plumbing) ships in a follow-up PR; for now the UI renders a countdown from `OneShotState`. |
| `CopilotEngage`     | `deck`                                                                         | AI owns the deck.                                  |
| `CopilotDisengage`  | `deck`                                                                         | User reclaims the deck.                            |
| `TakeOver`          | `deck`, `handoff_until_frame: u64`                                             | ADR-005 1-bar handoff. Control thread stamps frame. |
| `SetMasterLimiterEnabled`   | `enabled: bool`                                                          | Master-bus soft-clip limiter on/off. Default `true` (safety: live mix + recorded `master.wav` stay inside ±1.0). See `engine/src/audio/limiter.rs`. |
| `SetMasterLimiterThreshold` | `threshold_db: f32`                                                      | Master-bus limiter ceiling. Reducer clamps to `[-24.0, 0.0]`; non-finite → -0.5 default. Linear ceiling = `10^(db/20)`. |
| `SetSidechainEnabled`       | `enabled: bool`                                                          | Sidechain compressor on/off (#119). Default `false`. DSP wired in `engine/src/audio/mixer.rs` — when enabled, the non-trigger deck is ducked by the trigger deck's envelope. |
| `SetSidechainParams`        | `trigger_deck?, threshold_db?, ratio?, attack_ms?, release_ms?, makeup_gain_db?` | Sidechain compressor params (#119). All fields optional; `None` preserves prior value. Reducer clamps: threshold `[-60, 0]`, ratio `[1, 20]`, attack `[0.1, 100]` ms, release `[10, 2000]` ms, makeup `[0, 24]` dB. Non-finite values ignored. |

**Naming notes** (recorded once so future surfaces don't re-drift; see
issue #27):

* It's `PitchBend { semitones }`, **not** `PitchAdjust { value }` or
  `PitchAdjust { index }`. The unit is semitones; the engine clamps to
  ±12. **Independent of tempo** — set `TempoBend { ratio }` to change
  playback speed without changing key, or vice-versa. `PitchTempoReset
  { deck }` is a convenience that zeroes both at once.
* `EqAdjust` carries `value_db`, **not** `value`. Suffix is load-bearing
  because the engine clamps to a pro-DJ dB window.
* `HotCueSet` / `HotCueTrigger` address slots by `slot`, **not** `index`.
  Range is `0..8`.
* `Crossfader` is a single discrete `{ value }` (the engine schedules
  smooth ramps internally when the copilot emits `CrossfaderRamp` — that
  schema is documented separately under copilot protocol; it is **not** a
  wire event the engine accepts directly today).
* `SetCrossfaderCurve` carries `curve` as a **bare PascalCase variant
  name** (`"Linear"`, `"Dipped"`, `"Sharp"`, `"Scratch"`) — serde
  external-tag default. UIs that send a lowercased label or a numeric
  index will fail deserialization. Old snapshots that omit
  `crossfader_curve` deserialize to `Linear` (the
  `#[serde(default)]` on `EngineState::crossfader_curve`).

The deck identifier is `deck: "A" | "B"` everywhere — serde external
tag for a unit enum.

#### `DeckLoad` payload — beat-grid + downbeats

`DeckLoad` carries the pre-analyzed beat-grid for the incoming track.
The optional `downbeats_ms` array (added in the beat-grid analysis PR)
populates `Deck::downbeats` on the reducer side and drives
phrase-aligned transitions in the co-pilot.

```json
{
  "DeckLoad": {
    "deck": "B",
    "track": { "id": "trk-7", "path": "/music/foo.mp3" },
    "bpm": 124.0,
    "beat_grid_anchor_ms": 42,
    "downbeats_ms": [42, 1977, 3912, 5847, 7782],
    "hot_cues": [0, 1500, null, 8000, null, null, 60000, null]
  }
}
```

`downbeats_ms` is **optional** — pre-analysis payloads can omit the
field and the engine's serde default (empty list) leaves the deck with
no downbeat grid. The reducer truncates the array to the first **64**
entries before storing it on the `Deck` (see `DOWNBEATS_INLINE_CAPACITY`
in `engine/src/state.rs`); tracks with more downbeats still load, but
phrase alignment past the 64th bar falls back to bar-grid math derived
from `beat_grid_anchor_ms + 4 × beat_period_ms`.

`hot_cues` is **optional** and always exactly **8 slots** when present
— each slot is either a millisecond position (`u64`, track-relative)
or `null` for an empty slot. Sourced from the copilot library's
`hot_cues_json` column (see `library.set_hot_cues` below) so a track
always loads with the cues it was last saved with. Pre-hot-cue
payloads can omit the field and the engine's serde default (8 nulls)
leaves the deck with a fresh cue grid.

**Result**
```json
{ "accepted": true }
```

On success the bridge stamps the event with a monotonic id + `ts_micros`
and forwards it onto the control-loop event channel via a non-blocking
`try_send`. The reducer + audio dispatch happen on the control thread;
clients observe the resulting state via the subsequent
`engine.state_changed` notification, not as part of this response.

**Errors specific to this method**

| Code     | Symbol                | Meaning                                                                                                                    |
|----------|-----------------------|----------------------------------------------------------------------------------------------------------------------------|
| `-32000` | `ENGINE_OFFLINE`      | Control-loop event channel is full or its receiver was dropped (control thread exited). Caller may retry after backoff.    |
| `-32001` | `ENGINE_SINK_UNWIRED` | The serving `EngineHandle` was built without an event sink — common in unit tests using `EngineHandle::new()`. Other methods (`engine.snapshot`, `engine.event_log`, `engine.health`) still succeed. |
| `-32003` | `RATE_LIMITED`        | Per-client token bucket exhausted. The bridge caps `engine.submit_event` at **200 events/sec sustained** with a **1 000-event burst** per WS connection — a guard against malicious / buggy UIs that would otherwise starve the bounded control-loop channel. The error `data` field carries `{ "retry_after_ms": <u64> }`; clients should back off at least that long before retrying. The bucket refills continuously at one token per 5 ms (one token = one `engine.submit_event` frame). Set `HYPEHOUSE_RATE_LIMIT_DISABLED=1` in the server's env to disable the gate for dev/test workflows. |

### `submit_event` data path

```
   UI / MIDI / copilot           Rust engine process
   ┌────────────────┐
   │ submit_event   │            ┌──────────────────────────────┐
   │ DeckLoad{…}    │ ───WS──▶   │ bridge::ws_server            │
   └────────────────┘            │   dispatch SUBMIT_EVENT      │
                                 │   stamp id + ts              │
                                 │   engine.forward_event       │
                                 │     try_send(event)          │
                                 │       ├─ Ok        → result  │
                                 │       │              accepted│
                                 │       ├─ Full      → -32000  │
                                 │       └─ Disconn.  → -32000  │
                                 └──────────────┬───────────────┘
                                                ▼
                                 ┌──────────────────────────────┐
                                 │  crossbeam channel<Event>    │
                                 │   (control-plane back-pressure)
                                 └──────────────┬───────────────┘
                                                ▼
                                 ┌──────────────────────────────┐
                                 │ control_loop (OS thread)     │
                                 │   state = state.apply(ev)    │
                                 │   cmds = translator(state…)  │
                                 │   producer.try_push(cmd)     │
                                 └──────────────┬───────────────┘
                                                ▼
                                 ┌──────────────────────────────┐
                                 │ SPSC AudioRing               │
                                 │   (lock-free, no alloc)      │
                                 └──────────────┬───────────────┘
                                                ▼
                                 ┌──────────────────────────────┐
                                 │ cpal audio callback          │
                                 │   render → device            │
                                 └──────────────────────────────┘
```

The two queues (`crossbeam` event channel + `ringbuf` AudioRing) are the
boundary points where back-pressure is surfaced. The bridge maps the
control-plane queue to `-32000`; the audio-plane queue drops with a
`tracing` warn (no client-visible error today). Both choices avoid any
blocking on the WS task or the audio thread.

### `engine.snapshot`

Return the current `EngineState` as a single JSON object.

**Params**: none.
**Result**: a serialized `EngineState` — see `state::EngineState`.

### `engine.event_log`

Return a slice of the event log starting after `since` (exclusive).

**Params**
```json
{ "since": 0, "limit": 1024 }
```

`since` defaults to `0` (return from the beginning), `limit` defaults to
`1024`.

**Result**: `Vec<Event>` ordered by ascending `id`.

### `engine.health`

Liveness + telemetry probe. Returns counters scoped to the bridge.

**Params**: none.
**Result**
```json
{
  "uptime_ms": 12345,
  "audio_xrun_count": 0,
  "ws_clients_connected": 2,
  "ring_pending": 0
}
```

### `engine.list_effects` (ADR-006)

Returns the effect manifest — the catalogue of built-in effects the
audio engine ships with. Used by the UI to render the per-deck
effects-chain controls. Static for a given engine build.

The handler reads from `crate::audio::effects::descriptors()` so the
wire payload follows the engine registry without drift: a new built-in
effect added to the registry shows up here automatically.

**Params**: none.
**Result**: object with an `effects` array. Wrapping in an object
(rather than returning a bare array) keeps the response
forward-compatible with future top-level fields such as `version` or
`build_id` without breaking JSON-RPC clients.

```json
{
  "effects": [
    {
      "id": 1,
      "name": "filter",
      "params": [
        { "name": "cutoff_hz", "min": 20, "max": 20000, "default": 500 },
        { "name": "resonance", "min": 0, "max": 1, "default": 0.3 },
        { "name": "mode", "min": 0, "max": 2, "default": 0 }
      ]
    },
    {
      "id": 2,
      "name": "echo",
      "params": [
        { "name": "time_ms", "min": 10, "max": 2000, "default": 250 },
        { "name": "feedback", "min": 0, "max": 0.95, "default": 0.45 },
        { "name": "tone", "min": -1, "max": 1, "default": 0 }
      ]
    },
    {
      "id": 3,
      "name": "reverb",
      "params": [
        { "name": "room_size", "min": 0, "max": 1, "default": 0.5 },
        { "name": "damping", "min": 0, "max": 1, "default": 0.4 },
        { "name": "width", "min": 0, "max": 1, "default": 0.7 }
      ]
    },
    {
      "id": 4,
      "name": "gate",
      "params": [
        { "name": "period_div", "min": 0, "max": 3, "default": 1 },
        { "name": "duty", "min": 0, "max": 1, "default": 0.5 }
      ]
    }
  ]
}
```

Reserved `id` values:

| id | effect  | notes |
|----|---------|-------|
| 0  | (none)  | empty slot — `EffectAssign { effect_id: 0 }` clears |
| 1  | filter  | RBJ biquad LP / HP / BP |
| 2  | echo    | delay line + cross-feedback + tone tilt |
| 3  | reverb  | Schroeder 4-comb + 2-allpass |
| 4  | gate    | beat-synced gate (master BPM x period_div) |

### `engine.list_output_devices`

Enumerate cpal output device names so the UI can render a picker. Read-only;
the engine does **not** hot-swap on selection — the user persists the desired
substring in `HYPEHOUSE_OUTPUT_DEVICE` and restarts the engine. Live hot-swap
is deferred (ADR-TBD) because tearing down + rebuilding a cpal Stream under an
active audio thread is non-trivial.

Use case: livestreaming. Set `HYPEHOUSE_OUTPUT_DEVICE=BlackHole` (macOS) /
`VB-Cable` (Windows) / `pipewire-loopback` (Linux) → engine routes master mix
into the virtual sink → OBS / Twitch captures lossless audio without
screen-share loopback. See issue #111.

**Params**: none.
**Result**: object with a `devices` array. `is_default = true` flags the
host's current default output device (cpal canonical name match).

```json
{
  "devices": [
    { "name": "MacBook Pro Speakers", "is_default": true },
    { "name": "BlackHole 2ch", "is_default": false },
    { "name": "External Headphones (USB)", "is_default": false }
  ]
}
```

Defunct devices (cpal `name()` returns Err) are skipped. Empty array is
valid when the host has no audio sink (e.g. headless container).

### `engine.session_info`

Read-only snapshot of session-static info — engine version + active
output-device substring + feature flags. Pure: reads build-time
`CARGO_PKG_VERSION` plus the relevant env vars at call time. Useful
for UI "About" panels + debug toasts + diagnostic reports.

**Params**: none.
**Result**:

```json
{
  "version": "0.1.0",
  "output_device_substring": "BlackHole",
  "features": {
    "midi_clock_in":  false,
    "midi_clock_out": false,
    "ableton_link":   false,
    "sentry_telemetry": false,
    "recording_enabled": true,
    "rate_limit_disabled": false,
    "shared_ci_runner": false
  }
}
```

Field semantics:
- `version` — `CARGO_PKG_VERSION` of the running engine.
- `output_device_substring` — value of `HYPEHOUSE_OUTPUT_DEVICE`; empty
  when unset (engine uses host default).
- `features.midi_clock_in` / `.midi_clock_out` — `true` iff both the
  matching Cargo feature is compiled in AND the corresponding device
  env var is non-empty.
- `features.ableton_link` — same compile-feature AND env-flag gate.
- `features.sentry_telemetry` — `true` iff `SENTRY_DSN` is non-empty.
- `features.recording_enabled` — `true` unless
  `HYPEHOUSE_RECORDING_DISABLED` is set.
- `features.rate_limit_disabled` / `features.shared_ci_runner` —
  diagnostic exposures of the matching env vars.

### `engine.list_sessions`

Enumerate persisted past sessions on disk. Read-only — never touches
live engine state. Backed by
`crate::persistence::sessions::list_sessions`, which walks the
resolved persistence root (`$HYPEHOUSE_EVENT_LOG_DIR` →
`$XDG_DATA_HOME/hypehouse-live/sessions` → `~/.local/share/...`)
and returns one summary per directory. Used by the UI History panel.

> **Retention note**: The engine prunes stale session directories at
> boot — entries returned here are already filtered by the retention
> policy (default: drop directories older than 30 days, but always
> keep the 50 most-recent regardless of age). The sweep is **not** a
> protocol surface; it runs once per process start in `main.rs` after
> `EventLog::new`. Tuned via `HYPEHOUSE_LOG_MAX_DAYS`,
> `HYPEHOUSE_LOG_MIN_KEEP`, and `HYPEHOUSE_LOG_RETENTION_DISABLED=1`.
> See ADR-003 §Retention.

**Params**: none.
**Result**: object with a `sessions` array, sorted by `started_at_micros`
descending (most recent first). Capped at 50 entries. Sessions with an
unreadable or empty `events.jsonl` still appear (with
`event_count: 0`, `started_at_micros: null`) so the user can see the
directory exists.

```json
{
  "sessions": [
    {
      "id": "20260518T013312Z-a4f2",
      "started_at_micros": 1779067992000000,
      "ended_at_micros":   1779071512000000,
      "event_count": 4823,
      "has_recording": true,
      "recording_size_bytes": 2147483648
    }
  ]
}
```

* `id` — directory name; matches `crate::persistence::new_session_id()`
  format (`YYYYMMDDTHHMMSSZ-XXXX`).
* `started_at_micros` / `ended_at_micros` — `ts_micros` of the first /
  last event in `events.jsonl`. `null` when log is empty / unparseable.
* `event_count` — JSONL line count (non-empty lines).
* `has_recording` — `true` only when `master.wav` is present **and**
  non-empty.
* `recording_size_bytes` — `master.wav` size in bytes, or `null` when
  absent.

### `engine.replay_session`

Fold the events of a single past session through `EngineState::apply`
and return the resulting snapshot. **Read-only** — does NOT mutate live
engine state. v0.1 returns the snapshot for inspection / UI rendering;
a future PR can layer "load this snapshot into the engine" on top.

**Params**:

```json
{ "session_id": "20260518T013312Z-a4f2" }
```

`session_id` is validated to refuse path traversal (no `/`, `\`, `\0`,
no leading dot, length ≤ 128). Invalid ids return `-32602`.

**Result**:

```json
{
  "state": { "...": "full EngineState snapshot — same shape as engine.snapshot" },
  "event_count": 4823
}
```

A missing or empty `events.jsonl` returns `state = EngineState::default()`
+ `event_count = 0` rather than an error — the UI shows "no events
yet" without alarming the user.

Errors:

* `-32602 INVALID_PARAMS` — bad `session_id` shape / not found / file
  parse failure (malformed JSONL line). The error `data` field carries
  the underlying anyhow chain for debugging.

Replay correctness is bounded by the event-sourced contract: the
reducer is a pure fold over `Event`s, so any state field derivable
from events alone reconstructs exactly. Fields that depend on
wall-clock time (e.g. live track playhead position) are stamped at
event time and replayed verbatim — the snapshot reflects "where the
state was at the last event", not "where the audio is now". The
master mix audio (`master.wav`) is not replayed through this RPC; the
UI can offer the file path for direct playback.

## `library.*` namespace — engine-bridge proxy to copilot

All `library.*` JSON-RPC methods (`list_tracks`, `add_track`,
`search_tracks`, `add_track_from_directory`, `set_hot_cues`,
`get_waveform`, `compute_stems`, `get_stems`) are received by the
**engine bridge** on
`ws://127.0.0.1:8765` and forwarded over HTTP to the copilot service.
The UI therefore holds only **one** WebSocket — the engine — and never
opens a direct connection to the copilot.

Transport details:

* **Default copilot endpoint** — `http://127.0.0.1:8766/rpc`.
* **Override** — `HYPEHOUSE_COPILOT_URL` environment variable on the
  engine process. Set to the empty string to hard-disable the proxy
  (every `library.*` call then returns `-32000` with
  `data: "copilot proxy disabled"`).
* **Timeout** — 5 seconds per call. A hung copilot surfaces as
  `-32000 engine offline` with `data` carrying a `(timeout)` marker.
* **Auth** — the engine's WS auth gate (`auth.hello` for browser
  clients) is enforced **before** any library proxy hop, so an
  unauthenticated UI cannot trigger outbound HTTP traffic.
* **Error mapping** — JSON-RPC errors returned by the copilot are
  passed through verbatim (the original `code` / `message` / `data`).
  Network-layer failures collapse to `-32000 engine offline`.

The copilot's `LibraryRpcHandler` keeps its own native dispatch surface
(see `copilot/library_rpc.py`) for direct integrations — the proxy is
an additional entry point, not a replacement.

The HTTP listener on the copilot side is implemented in
`copilot/http_server.py` (`JsonRpcHttpServer`). It exposes:

* `POST /rpc` — accepts a JSON-RPC 2.0 request, dispatches to the
  registered handlers (today: `LibraryRpcHandler`), returns a JSON-RPC
  2.0 response. HTTP status is always 200; failure is carried in the
  body's `error` field (`-32700` parse, `-32600` invalid envelope,
  `-32601` unknown method, `-32602` invalid params, `-32603` internal).
* `GET /health` — returns `{"status": "ok", "service":
  "hypehouse-copilot"}` for the engine's liveness check before it
  routes proxy traffic.

The copilot binds the listener via `CoPilotService.run_with_http_server()`
which `asyncio.gather`s the HTTP server with the engine WS subscriber.
Pass `--no-http-server` on the copilot CLI to run subscriber-only.

### `library.list_tracks` (co-pilot)

Paginated dump of the co-pilot's SQLite track catalog. Exposed by
`copilot.library_rpc.LibraryRpcHandler`; the UI calls it on mount to
populate the Library panel.

**Params** (all optional)

```json
{ "limit": 100, "offset": 0 }
```

`limit` is clamped silently to `[1, 1000]`; `offset` is clamped to `>= 0`.

**Result**

```json
{
  "tracks": [
    {
      "id": "kanye-stronger",
      "path": "/music/kanye-stronger.mp3",
      "bpm": 124.0,
      "camelot_key": "8B",
      "energy": 0.21,
      "duration_s": 265.3,
      "beat_grid_anchor_ms": 0,
      "beat_period_ms": 483.87,
      "downbeats_ms": [0, 1935, 3870]
    }
  ],
  "total": 1,
  "limit": 100,
  "offset": 0
}
```

The `id` / `path` pair is wire-compatible with `state::TrackRef` so a
returned row can be passed straight into a `DeckLoad` event's `track`
field.

### `library.search_tracks` (co-pilot)

Substring + shorthand filter search. Shorthand tokens AND together with
substring tokens.

| Token            | Match                                                    |
|------------------|----------------------------------------------------------|
| `foo` (default)  | case-insensitive substring on `track_id` or `path`       |
| `key:8B`         | exact Camelot key match                                  |
| `bpm:120-130`    | inclusive BPM range                                      |

**Params**: `{ "query": "<string>", "limit": 100 }` — empty query returns
the first `limit` rows in alphabetical id order.

**Result**: `{ "tracks": [...], "query": "<echo>", "limit": <int> }`.

### `library.add_track` (co-pilot)

Run the analyzer on a single local file and persist the result.

**Params**: `{ "path": "/absolute/path/to/file.mp3" }`.

**Errors**: `-32602` if `path` is missing or the file doesn't exist /
isn't a regular file; `-32603` if the analyzer raises.

**Result**: `{ "track": <TrackRef wire shape> }`.

### `library.add_track_from_directory` (co-pilot)

Recursively scan a server-side directory and analyze every supported
file (`.mp3`, `.wav`, `.flac`, `.m4a`, `.aac`, `.ogg`). Idempotent —
files already in the catalog (matched by stem) are skipped. Used by
the UI's empty-state because the browser file picker can't surface
server-resolvable paths.

**Params**: `{ "path": "/absolute/path/to/music/dir" }`.

**Errors**: `-32602` on bad path; `-32603` on analyzer failure.

### `library.set_hot_cues` (co-pilot)

Persist an updated 8-slot hot-cue grid for a single track. Drives the
UI's hot-cue persistence path — every `HotCueSet` engine event the user
fires on a library track is debounced (~500ms) into one call to this
method so a track always reloads with the cues it was last saved with.

**Params**:

```json
{
  "track_id": "kanye-stronger",
  "hot_cues": [0, 1500, null, 8000, null, null, 60000, null]
}
```

* `track_id` — library row id (matches the wire `id` field, not the
  filesystem path).
* `hot_cues` — exactly **8 slots**. Each slot is either a
  non-negative integer (ms position relative to track start) or
  `null` for an empty slot.

**Result**:

```json
{ "track": <TrackRef wire shape> }
```

The returned `track` is the freshly-persisted row including the new
`hot_cues` array — UI caches can swap it in without a follow-up
`library.list_tracks` fetch.

**Errors**:

| Code     | Reason                                                            |
|----------|-------------------------------------------------------------------|
| `-32602` | `track_id` missing / not in the catalog.                          |
| `-32602` | `hot_cues` wrong length (must be exactly 8), wrong type, negative, or `bool` slot value. |

**Result**: `{ "added": [...], "added_count": <int>, "total": <int> }`.

### `library.get_waveform` (co-pilot)

Return packed min/max peak pairs used by the UI's `Waveform` canvas
to draw a real waveform (instead of the v0.1 placeholder flat line).
Peaks are computed copilot-side at ingest time (see
`copilot/waveform.py`) and stored in the `waveform_peaks` BLOB column.
Tracks that pre-date schema v4 (or were inserted without peaks via
the test path) trigger a lazy compute on first request.

**Params**:

```json
{ "track_id": "kanye-stronger" }
```

**Result**:

```json
{
  "track_id": "kanye-stronger",
  "peaks_b64": "AAECAwQF..."
}
```

* `peaks_b64` — base64-encoded packed peak-pairs bytes. Layout is
  `[min_0, max_0, min_1, max_1, ...]` where each value is an `i8` in
  `[-128, 127]` mapping audio `[-1.0, 1.0]`. Default 2000 buckets ⇒
  4000 bytes raw ⇒ ~5400 b64 chars.
* `peaks_b64` is `null` when the track is unknown, when peaks haven't
  been computed yet, or when a lazy-compute attempt failed (file
  moved, codec missing). The UI's `null` branch falls back to the
  flat-line render so this is a graceful degradation rather than an
  error.

**Errors**: `-32602` if `track_id` is missing / empty. Unknown
`track_id` is *not* an error — it returns `peaks_b64: null`.

### `library.compute_stems` (co-pilot)

Kick off **stem separation** for a track — run Facebook's `demucs`
model to split the track into four mono/stereo WAVs (`vocals.wav` /
`drums.wav` / `bass.wav` / `other.wav`). See [`docs/stems.md`](../stems.md)
for the design + perf budget.

The actual compute is heavy (~30 s on GPU, ~3 min on CPU), so the call
returns **immediately** with a `pending` envelope and the work runs in
a background asyncio task. The UI polls `library.get_stems` for
completion.

**Params**:

```json
{ "track_id": "kanye-stronger" }
```

**Result**:

```json
{ "track_id": "kanye-stronger", "status": "pending" }
```

Calling this method again while a task is already in flight is a no-op
— the same `{status: "pending"}` envelope comes back without
scheduling a second demucs run.

**Errors**:

| Code     | Reason                                                            |
|----------|-------------------------------------------------------------------|
| `-32602` | `track_id` missing or unknown.                                    |
| `-32000` | Optional `demucs` dependency not installed (`pip install hypehouse-copilot[stems]`). |

### `library.get_stems` (co-pilot)

Poll the current stem-cache state for a track. Returns the four WAV
paths once stem separation has completed.

**Params**:

```json
{ "track_id": "kanye-stronger" }
```

**Result** (ready):

```json
{
  "track_id": "kanye-stronger",
  "status": "ready",
  "stems": {
    "vocals": "/home/sandy/.local/share/hypehouse-live/stems/kanye-stronger/vocals.wav",
    "drums":  "/home/sandy/.local/share/hypehouse-live/stems/kanye-stronger/drums.wav",
    "bass":   "/home/sandy/.local/share/hypehouse-live/stems/kanye-stronger/bass.wav",
    "other":  "/home/sandy/.local/share/hypehouse-live/stems/kanye-stronger/other.wav"
  }
}
```

**Result** (pending / failed / never requested):

```json
{ "track_id": "kanye-stronger", "status": "pending", "stems": null }
```

* `status: "pending"` — `library.compute_stems` was called and the
  task is still running.
* `status: "failed"` — demucs raised, OR a previously-`ready` cache
  was nuked from disk between requests. The UI can offer a retry by
  re-calling `library.compute_stems`.
* `status: null` — the track exists but stems have never been
  requested.
* Missing track (unknown `track_id`) returns
  `{status: null, stems: null}` rather than an error envelope —
  mirrors the `library.get_waveform` "missing = graceful null"
  convention so the UI's single fetch path stays simple.

**Errors**: `-32602` if `track_id` is missing / empty.

### `library.sync_status` (co-pilot)

Snapshot of cloud-sync state (see [docs/cloud-sync.md](../cloud-sync.md)
for the operator guide). Cheap — reads cached daemon counters under a
lock; never round-trips the cloud.

**Params**: none.

**Result**:

```json
{
  "pending_push_count":  4,
  "library_track_count": 137,
  "last_pull_micros":    1700000060000000,
  "last_push_micros":    1700000060000000,
  "last_pull_fetched":   2,
  "last_pull_applied":   2,
  "last_push_pushed":    1,
  "last_tick_error":     "",
  "next_sync_micros":    1700000120000000
}
```

All `*_micros` fields are wall-clock micros (UNIX epoch). `0` before
the daemon's first tick. `next_sync_micros` is owned by the daemon's
loop and reflects the actual scheduled wake — out-of-band callers
don't move it (intentionally — see #174 / #176 design notes).

Daemon-less mode (no Supabase env vars) returns the two counts with
all stats fields zeroed.

### `library.sync_now` (co-pilot)

Operator-driven force tick. Runs an out-of-band pull+push and wakes
the daemon thread so its next automatic tick fires at the reset
cadence rather than waiting out the prior backoff window.

**Params**: none.

**Result**: identical shape to `library.sync_status`, reflecting the
post-tick state.

**Errors**:
- `-32000 cloud sync not configured` — no `SyncDaemon` wired
  (Supabase env vars absent).
- `-32603 cloud sync transport error: <msg>` — Supabase / network
  failure during the tick.
- `-32603 cloud sync local DB error: <msg>` — SQLite hiccup
  (lock contention, malformed schema, etc.).

### `library.list_pending_push` (co-pilot)

Returns the set of track IDs awaiting cloud push. Backs the UI's
per-row "⟳ pending" chip + "Pending sync only" filter.

**Params**: none.

**Result**:

```json
{ "ids": ["kanye-stronger", "rkfd-keys-of-life"] }
```

Returned as a list (JSON has no native set type); UI builds a Set
client-side for O(1) membership checks.

### `library.requeue_all_pending` (co-pilot)

Operator escape hatch — enqueues every local track for cloud push.
Used after a pre-cloud-sync upgrade to seed a fresh Supabase project
from an existing local library. Idempotent: tracks already in the
queue keep their original `queued_at_micros` ordering.

Wakes the daemon with `skip_next_tick=False` so the freshly enqueued
rows drain on the next iteration rather than sitting through the
prior backoff window.

**Params**: none.

**Result**:

```json
{ "queued": 137 }
```

`queued` is the **total** pending-push count after the call, not the
number of newly added rows.

### `library.stems_status` (co-pilot)

Aggregate counts of tracks by demucs stems-status. Backs the
AboutPanel "Stems" row.

**Params**: none.

**Result**:

```json
{ "ready": 8, "pending": 1, "failed": 0, "none": 128 }
```

All four buckets are always present (zeros when empty). `"none"`
covers BOTH tracks whose `tracks.stems_status` is NULL and tracks
whose status is an unknown future enum value (defensive bucket —
unknown values never silently disappear).

## Server-pushed notifications

Notifications have no `id` and expect no response.

### `engine.state_changed`

Emitted after every accepted `engine.submit_event` (and after any other
in-process call that mutates state — audio thread, MIDI listener, etc.).

```json
{
  "jsonrpc": "2.0",
  "method": "engine.state_changed",
  "params": {
    "state": <EngineState>,
    "last_event_id": 17,
    "master_limiter_gain_reduction_db": -0.5,
    "sidechain_gain_reduction_db": -3.2,
    "clock_source": "internal",
    "perf": { "cpu_percent": 12.4, "render_p99_us": 480, "underrun_count": 0 }
  }
}
```

Live audio-thread side-channel fields (NOT part of the event-sourced
reducer — sampled from atomics at notification time):

* `master_limiter_gain_reduction_db` — current GR on the master-bus
  soft-clip limiter. `≤ 0`. Drives the UI master limiter meter.
* `sidechain_gain_reduction_db` — current GR on the sidechain
  compressor (#119). `≤ 0` when ducking, `0` when bypassed. Drives
  the UI ducking meter.
* `clock_source` — `"internal" | "midi_in" | "ableton_link"`. UI
  BPM-lock badge keys off this.
* `perf` — CPU%, render p99 latency, underrun count. UI perf dashboard.

### `engine.audio_alert`

Out-of-band hardware / xrun notice from the audio thread. Surfaced so
the UI can warn the operator without polling.

```json
{
  "jsonrpc": "2.0",
  "method": "engine.audio_alert",
  "params": { "kind": "xrun", "details": "cpal callback underran by 2.3ms" }
}
```

`kind` is a free-form string today (`"xrun"`, `"underrun"`, etc.); the
set may be tightened to an enum once the audio thread lands.

### `engine.decode_error`

Surfaces a decode-pipeline failure on a `DeckLoad` or
`DeckLoadStems` event. The Rust engine's `DecodeService::open` /
`DecodeService::open_stems` calls may fail for several reasons
(file not found, unsupported format, exhausted decode slots, …).
Instead of silently dropping the load, the bridge fans out an
`engine.decode_error` notification so connected UIs can render a
transient toast and the operator immediately sees what went wrong.
Deck state stays unchanged — this notification is a side channel,
not a reducer event.

```json
{
  "jsonrpc": "2.0",
  "method": "engine.decode_error",
  "params": {
    "deck": "A",
    "track_id": "abc-123",
    "category": "file_not_found",
    "error": "io error opening /tracks/missing.mp3: No such file or directory (os error 2)"
  }
}
```

`category` is a coarse, stable failure-class string the UI uses to pick
icons / copy without pattern-matching the underlying error message.
Today's set, with the underlying `DecodeError` variant it maps from:

| `category`                  | Source                                | Surfaces                                                                  |
|-----------------------------|---------------------------------------|---------------------------------------------------------------------------|
| `file_not_found`            | `DecodeError::Io`                     | Open-time IO error (path missing, perms denied, etc.).                    |
| `format_unsupported`        | `DecodeError::Probe`, `NoTrack`       | `symphonia` couldn't probe the container or no decodable track.           |
| `decoder_error`             | `DecodeError::Resampler`              | Decoder thread init failed at open (rubato config invalid).               |
| `resource_exhausted`        | `DecodeError::NoFreeSlot`             | All `MAX_DECODE_SLOTS` slots occupied; close a deck and retry.            |
| `unknown_inline_source`     | `DecodeError::UnknownInlineSource`    | Test/in-memory `mem://` key not registered.                               |
| `decoder_thread_spawn`      | `DecodeError::Spawn`                  | OS refused to spawn the per-track decoder thread.                         |
| `mid_stream_decode_failure` | Decoder-thread `MidStreamFailureKind::DecodeFailed` / `ResampleFailed` | After-open failure: symphonia returned a non-recoverable error mid-track, or rubato resample failed on a mid-stream chunk. The decoder thread exits cleanly; the audio thread silence-pads the now-quiet ring. |
| `decoder_thread_panic`      | Decoder-thread `catch_unwind`         | The per-track decoder thread itself panicked. The unwind is caught, the panic message surfaces in `error`, and the audio thread continues without crashing. |

`error` is the human-readable stringification of the underlying
failure and is meant for display + log capture, not for parsing.

The first six rows are produced **synchronously** by the control
thread when `DecodeService::open` (for `DeckLoad`) or
`DecodeService::open_stems` (for `DeckLoadStems`) returns `Err`.

The last two rows (`mid_stream_decode_failure` + `decoder_thread_panic`)
are produced **asynchronously** by the decoder thread once it has
already been spawned. The decoder thread pushes a `MidStreamFailure`
onto a bounded `crossbeam::channel` (capacity 64); a tokio drain
task on the bridge polls the receiver every 100 ms and broadcasts an
`engine.decode_error` notification for each one. Backpressure: a full
channel drops the event with a `warn!` rather than blocking the
decoder thread (which must never block on a slow consumer).

Clients should treat `category` as a forward-compatible union and
fall back to a generic "Decode error" label for unknown values.

## Error codes

Standard JSON-RPC 2.0 codes:

| Code     | Symbol            | Meaning                                                              |
|----------|-------------------|----------------------------------------------------------------------|
| `-32700` | `PARSE_ERROR`     | Defined in spec; reserved. The engine currently maps malformed-JSON to `-32600` per its framing contract. |
| `-32600` | `INVALID_REQUEST` | Payload is not a valid JSON-RPC 2.0 request, or framing failed.      |
| `-32601` | `METHOD_NOT_FOUND`| Unknown method.                                                      |
| `-32602` | `INVALID_PARAMS`  | Method exists but params shape is wrong / fails deserialization.     |
| `-32603` | `INTERNAL_ERROR`  | Reducer / serializer fault.                                          |

Application-defined codes live in `-32000..=-32099`:

| Code     | Symbol                | Meaning                                                                                                                                                          |
|----------|-----------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `-32000` | `ENGINE_OFFLINE`      | The bridge could not forward an event onto the control-loop event channel (full or disconnected). Caller may retry after backoff. Emitted from `engine.submit_event`. |
| `-32001` | `ENGINE_SINK_UNWIRED` | The serving `EngineHandle` was built without an event sink (`EngineHandle::new()` instead of `EngineHandle::with_event_sink(tx)`). Other RPCs still work.        |
| `-32002` | `AUTH_REJECTED`       | In-protocol auth rejection. Returned when (a) a `PendingAuth` browser-mode connection calls any method other than `auth.hello`, or (b) `auth.hello` is called with an invalid token. Native clients that present a wrong `Authorization` header still get HTTP 401 at the WS handshake instead — they never reach this code path. |
| `-32003` | `RATE_LIMITED`        | Per-client token bucket on `engine.submit_event` is exhausted. The bridge caps inbound `engine.submit_event` frames at **200 events/sec sustained** with a **1 000-event burst**, per WS connection. The bucket is consumed BEFORE dispatch so a flood cannot drain the bounded control-loop channel. The `data` field carries `{ "retry_after_ms": <u64> }` — the minimum wait before the next token regenerates. Other methods (`engine.snapshot`, `engine.health`, `auth.hello`, `library.*`, …) are NOT rate-limited. Setting `HYPEHOUSE_RATE_LIMIT_DISABLED=1` in the server's env disables the gate entirely (intended for dev/test, never production). |

Error envelope:

```json
{
  "jsonrpc": "2.0",
  "error": { "code": -32601, "message": "Method not found", "data": "engine.no_such" },
  "id": 1
}
```

`data` is optional and currently always a string describing the
specific cause (e.g., the unknown method name, the serde error).

## Multi-client fan-out

The bridge holds a `tokio::sync::broadcast::Sender<BridgeNotice>`
internally. Every connected client task subscribes; every accepted
`submit_event` produces exactly one broadcast, and every subscriber
sends it down its own WS write half. The UI, copilot, and any
diagnostic / mobile-remote client all see the same notification stream
with no cross-talk.

## Shutdown

`tokio::signal::ctrl_c` (and SIGTERM on Unix) trips the cancel oneshot.
The accept loop exits, in-flight client tasks drain, the server task
returns, and `engine`'s `main` exits zero. Clients see their WS close
frame and can reconnect.

## Beat-grid + downbeats

Phrase-aligned transitions need three pieces of timing data per track:

* **`bpm`** — used to derive the beat period (`60_000 / bpm` ms).
* **`beat_grid_anchor_ms`** — ms position of beat 0 inside the track.
  Most tracks have a near-zero anchor; outliers happen when a track
  starts with a silent / non-rhythmic intro and the analyzer locks the
  grid to the first detected beat.
* **`downbeats_ms`** — ms positions of bar-starts (every 4 beats for
  4/4 material). Sourced from the co-pilot's
  `copilot.vendor.analyzer.detect_downbeats` (madmom DBNBeatTracker)
  pass during library ingest; persisted to the SQLite catalog and
  forwarded on `DeckLoad`.

The co-pilot's `TransitionProposer` consumes `Deck::downbeats` on the
**outgoing** deck to pick the next bar boundary for the crossfade
start. The math lives in `copilot.proposer.next_downbeat_after`:

```
beat_align_at_ms = next_downbeat_after(
    playing_deck.position_ms, playing_deck.downbeats
)
# Fallback when no future downbeat exists (track is in its outro):
# beat_align_at_ms = playing_deck.position_ms
```

The result is published on the `Proposal.transition_plan.beat_align_at_ms`
field; the engine uses it to schedule the `CrossfaderRamp` start so the
ramp begins on a bar. Tracks without downbeat data (legacy library
rows, analysis still pending) emit `beat_align_at_ms = position_ms`,
which still works but loses phrase alignment.

### Engine-side storage

`Deck::downbeats: SmallVec<[u32; 64]>` — inline-allocated for the
common 3-5 minute track at typical tempos. Tracks with more than 64
downbeats are truncated to the first 64; the doc comment on the field
calls out the truncation. The audio thread reads the grid from the
lock-free snapshot of `EngineState`; no allocation happens on the hot
path.

`u32` ms ceiling = ~71 minutes, well beyond any sane DJ track.

## Session recording (master.wav)

The engine writes the per-session master mix to
`<session_dir>/master.wav` (alongside `events.jsonl` from ADR-003).
Format is PCM IEEE-float stereo, 32-bit, sample rate = audio device's
preferred rate. The recorder is **not** a WS protocol surface; it's
documented in [`docs/recording.md`](../recording.md). Disable with
`HYPEHOUSE_RECORDING_DISABLED=1`.

## MIDI clock OUT (ADR-007 v0.1)

The engine can act as a MIDI clock **master**, emitting 24 PPQN MIDI
realtime messages so external hardware (drum machines, synths, MPCs,
modular sequencers) lock to the session tempo. **There is no protocol
surface** — MIDI clock OUT does not flow through the WebSocket bridge.
Documented here because it shares the master-tempo abstraction with the
event log.

### Enabling

| Knob | Value | Effect |
|---|---|---|
| Cargo feature | `midi-clock-out` (default **off**) | Compiles the `midir` output binding. Without it the env var is logged and ignored. |
| Env var       | `MIDI_CLOCK_OUT_DEVICE=<substring>`  | Case-insensitive substring matched against MIDI output port names. Empty / unset = disabled. |

```bash
# Native build with MIDI clock OUT, locking to the first port whose
# name contains "Maschine".
MIDI_CLOCK_OUT_DEVICE=maschine cargo run --features midi-clock-out
```

### Tempo source

Master tempo lives in `EngineState::master_bpm` (default 120.0) and is
mirrored into a lock-free `SharedClock::master_bpm()` atomic so the
clock-out tick thread can re-derive its period every iteration without
locking.

Update master tempo via a new event kind:

```json
{
  "jsonrpc": "2.0",
  "method": "engine.submit_event",
  "params": {
    "kind": { "SetMasterBpm": { "bpm": 128.0 } }
  },
  "id": 7
}
```

The reducer validates the value (`f32::is_finite && > 0.0`); bad inputs
are silently ignored. The MIDI clock OUT tick thread picks up the new
period within one tick (≤ ~21 ms at 120 BPM, ≤ ~17 ms at 144 BPM).

### Wire format

Single-byte MIDI realtime messages, no data bytes:

| Byte  | Name  | When |
|------:|-------|------|
| 0xFA  | Start | Once when the worker thread enters its run loop (i.e. when `MidiClockOut::start` succeeds). |
| 0xF8  | Clock | Every `60_000_000 / (bpm × 24)` µs. 24 PPQN per MIDI spec. |
| 0xFC  | Stop  | On `Drop` of the `MidiClockOut` handle (engine shutdown). |

No SongPositionPointer (0xF2) — v0.1 doesn't have a transport "play
head" concept; downstream gear assumes start-from-zero on 0xFA.

### Failure modes (non-fatal)

* `MIDI_CLOCK_OUT_DEVICE` set but the feature is disabled → log warn,
  continue without MIDI clock.
* No MIDI output ports available → log warn, continue.
* Substring matched no port → log warn, continue.
* `midir` connection failed → log warn, continue.

The engine never refuses to boot because of a missing MIDI device — DJ
rigs frequently start without all hardware plugged in.

## MIDI clock IN (ADR-007 v0.3)

The engine can act as a MIDI clock **slave**, locking its master
tempo (`SharedClock::master_bpm`) to a hardware sequencer / DAW that
emits 24 PPQN MIDI clock. **There is no protocol surface** — MIDI
clock IN does not flow through the WebSocket bridge. Documented here
because it mutates the same master-tempo abstraction that the event
log + MIDI clock OUT depend on.

### Enabling

| Knob | Value | Effect |
|---|---|---|
| Cargo feature | `midi-clock-in` (default **off**) | Compiles the `midir` input binding. Without it the env var is logged and ignored. |
| Env var       | `MIDI_CLOCK_IN_DEVICE=<substring>`  | Case-insensitive substring matched against MIDI input port names. Empty / unset = disabled. |

```bash
# Native build with MIDI clock IN, locking to the first input port
# whose name contains "IAC" (macOS IAC bus).
MIDI_CLOCK_IN_DEVICE=iac cargo run --features midi-clock-in
```

### Mode interaction with MIDI clock OUT

When `MIDI_CLOCK_IN_DEVICE` is set (and the feature is active), the
engine **silently disables** MIDI clock OUT to avoid a feedback loop
where the engine echoes the master's own clock back to it.

```bash
# IN takes precedence over OUT. OUT will not start.
MIDI_CLOCK_IN_DEVICE=iac \
  MIDI_CLOCK_OUT_DEVICE=maschine \
  cargo run --features midi-clock-in,midi-clock-out
```

A future v0.4 may add a "mirror" mode that re-emits the incoming
clock byte-for-byte; today the simpler interlock above ships.

### BPM derivation + smoothing

* **24 PPQN**: per the MIDI clock spec, 24 ticks = one quarter note.
* **Beat anchor**: the first 0xF8 after 0xFA timestamps the anchor.
  The next `TICKS_PER_BEAT (=24)` 0xF8 bytes complete one beat. We
  compute `BPM = 60.0 / beat_duration_secs`.
* **Smoothing window**: 4 most-recent beat-BPMs, mean-averaged before
  being pushed into `SharedClock::set_master_bpm`. ≈ 2 s of history
  at 120 BPM — enough to absorb USB-MIDI jitter, fast enough to
  follow a live tempo nudge within a beat or two.
* **Deadband**: smoothed BPM within ±0.1 BPM of the current
  `SharedClock::master_bpm` is dropped (no atomic store). ±0.1 BPM
  is the JND for a trained ear and well below consumer-grade MIDI
  USB timing precision.
* **Plausibility clamp**: inferred BPMs outside [20.0, 999.0] (e.g.
  a missed tick stretching the interval to 60 s) are rejected rather
  than poisoning the smoothing buffer.

### Wire format consumed

Single-byte MIDI realtime messages:

| Byte  | Name  | Effect |
|------:|-------|--------|
| 0xFA  | Start | Begin counting ticks; reset smoothing state. |
| 0xF8  | Clock | Inside a Start..Stop window: timestamp + count. Outside: ignored (some DAWs like Ableton Live emit 0xF8 continuously with transport stopped). |
| 0xFC  | Stop  | Stop counting; clear smoothing state. Subsequent 0xF8 are ignored until the next 0xFA. |

### Failure modes (non-fatal)

* `MIDI_CLOCK_IN_DEVICE` set but the feature is disabled → log warn,
  continue without MIDI clock IN.
* No MIDI input ports available → log warn, continue.
* Substring matched no port → log warn, continue.
* `midir` connection failed → log warn, continue.

As with clock OUT, the engine never refuses to boot because of a
missing MIDI input device.

## Test coverage

Unit + integration tests live under `engine/src/bridge/*` (per-module
unit) and `engine/tests/ws_bridge_integration.rs` (one full end-to-end
case). Coverage:

* submit_event with a valid DeckPlay → applied + state_id incremented.
* submit_event with malformed JSON → `-32600`.
* submit_event with unknown method → `-32601`.
* snapshot returns the current state.
* state_changed notification fires after submit_event.
* `HYPEHOUSE_BRIDGE_TOKEN` set + missing header → handshake rejected.
* `HYPEHOUSE_BRIDGE_TOKEN` unset → loopback bind, no auth required.
* Two simultaneous clients both see the same state_changed.
* Graceful `shutdown()` returns promptly with no clients.
* End-to-end integration: spin server on ephemeral port → connect →
  submit DeckPlay → assert response + notification + snapshot reflects.
* `engine.submit_event` forwarded onto control-loop channel and the
  matching `Event` lands on the receiver (full event-shape round-trip).
* `engine.submit_event` returns `-32000 engine offline` once the bounded
  event channel is saturated.
* `engine.submit_event` returns `-32001 engine sink not wired` when the
  handle was built without an event sink.

In-band auth (`engine/tests/ws_auth_hello.rs`):

* Connect without `Authorization` header → handshake accepted.
* `engine.submit_event` before `auth.hello` → `-32002 AUTH_REJECTED`,
  engine state untouched.
* `auth.hello` with valid token → `{authed: true, session: …}`; follow-up
  `submit_event` succeeds.
* No frames for >5s while pending-auth → server closes with WS code
  `1008` (`Policy`).
* Invalid `auth.hello` → `-32002`; retry with valid token still works.
* Header-authed native client skips `auth.hello` entirely (back-compat).


## Telemetry

The bridge does **not** receive or forward telemetry events. Telemetry
(opt-in Sentry crash + perf monitoring) is a per-process concern: the
engine, copilot, and UI each carry their own SDK and emit events
directly to the configured DSN.

See [`docs/telemetry.md`](../telemetry.md) for the privacy contract,
the opt-in flags (`HYPEHOUSE_TELEMETRY_ENABLED`,
`VITE_TELEMETRY_ENABLED`, `window.__HYPEHOUSE_TELEMETRY_ENABLED__`),
and what gets scrubbed before send. **Default is OFF.** No DSN is
contacted and no events leave the machine unless the operator has
explicitly enabled telemetry.
