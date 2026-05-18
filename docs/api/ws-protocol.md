# WebSocket bridge — JSON-RPC 2.0 protocol

**Status**: v0.1 (engine ↔ UI ↔ copilot)
**Owner**: engine crate, module `bridge`
**Source of truth**: `engine/src/bridge/rpc.rs` (method dispatch),
`engine/src/state.rs` (`Event` / `EventKind` / `EngineState` types).

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

## Server-pushed notifications

Notifications have no `id` and expect no response.

### `engine.state_changed`

Emitted after every accepted `engine.submit_event` (and after any other
in-process call that mutates state — audio thread, MIDI listener, etc.).

```json
{
  "jsonrpc": "2.0",
  "method": "engine.state_changed",
  "params": { "state": <EngineState>, "last_event_id": 17 }
}
```

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
