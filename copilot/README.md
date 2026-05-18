# `copilot` — HypeHouse Live AI co-pilot service

Python 3.11 long-running service. Connects to the Rust audio engine over
JSON-RPC/WebSocket; when the user toggles co-pilot mode on a deck, picks
the next track and plans the transition.

ADRs that govern this module:

- [ADR-002 — Deck primitive (co-pilot semantics)](../docs/adr/ADR-002-deck-primitive.md)
- [ADR-003 — Event-sourced state](../docs/adr/ADR-003-event-sourced-state.md)
- [ADR-005 — Takeover envelope](../docs/adr/ADR-005-takeover-envelope.md)

## Install + run

```bash
cd copilot
python -m venv venv && source venv/bin/activate
pip install -e .
python -m copilot --engine-ws ws://127.0.0.1:8765
```

Env vars:

| Var | Default | Purpose |
|---|---|---|
| `HYPEHOUSE_ENGINE_WS` | `ws://127.0.0.1:8765` | Engine WebSocket URL. |
| `HYPEHOUSE_LIBRARY_DB` | `~/.hypehouse-live/library.db` | SQLite library path. |
| `HYPEHOUSE_COPILOT_LOG_LEVEL` | `INFO` | Log level. |
| `HYPEHOUSE_COPILOT_HTTP_PORT` | `8766` | Bind port for the JSON-RPC HTTP server. |

## HTTP JSON-RPC server

The copilot exposes an HTTP endpoint that the engine bridge proxies
`library.*` calls to. See `docs/api/ws-protocol.md` ("`library.*`
namespace — engine-bridge proxy to copilot") for the engine-side
proxy contract.

| Endpoint | Method | Purpose |
|---|---|---|
| `/rpc` | POST | JSON-RPC 2.0 request → response. Dispatches `library.*` to `LibraryRpcHandler`; unknown methods return `-32601`. |
| `/health` | GET | Liveness probe — returns `{"status": "ok", "service": "hypehouse-copilot"}`. |

Default bind: `127.0.0.1:8766`. Override port via
`HYPEHOUSE_COPILOT_HTTP_PORT`. Disable entirely with `--no-http-server`
on the CLI (pure WS-subscriber mode — engine `library.*` proxy returns
`-32000 engine offline`).

```bash
# default: HTTP RPC + engine WS subscriber both active
python -m copilot

# pure subscriber mode (no HTTP RPC listener)
python -m copilot --no-http-server
```

## Tests

```bash
pip install -e ".[test]"
pytest tests/
```

## Layout

| Module | Purpose |
|---|---|
| `decisions.py` | Pure decision functions: `mashability_score`, `next_track_decision`, `transition_plan`. No I/O. |
| `library.py` | SQLite-backed track catalog + Camelot/BPM gate logic. |
| `schemas.py` | Pydantic mirrors of the Rust engine's serde shapes. |
| `service.py` | aiohttp WebSocket loop. Subscribes, reconnects with backoff, calls the decision functions. |
| `http_server.py` | aiohttp JSON-RPC 2.0 HTTP server (`/rpc`, `/health`). Receives `library.*` proxy hops from the engine bridge. |
| `library_rpc.py` | Transport-agnostic `library.*` dispatch handler. |
| `main.py` / `__main__.py` | `python -m copilot` entry. |
| `vendor/` | Verbatim copy of HypeHouse v1 `analyzer.py`, `mashup.py`, `shared_cache.py`. See `vendor/VENDOR.md`. |

## Wire shape

The service uses JSON-RPC 2.0 framing. Outbound:

- `engine.subscribe` `{ "topics": ["engine.state_changed"] }` — sent right
  after connect.
- `engine.submit_event` `{ "event": <Event> }` — emitted per event in the
  transition plan.

Inbound:

- `engine.state_changed` notification with `{ "state": <EngineState> }`
  payload — engine pushes after every reducer call.

## v0.1 limitations (documented in PR body)

- `transition_plan` is stubbed: fixed 16-bar crossfade, no tempo/pitch
  matching, no stem-aware EQ swap.
- `mashability_score` weights are heuristic; will be tuned against real
  session logs once we have any.
- Library schema is minimal — analysis sidecars from v1 (downbeats,
  segments, drop_times) aren't materialised yet.
- Engine state-changed payload is a full snapshot per change; a delta
  protocol replaces this in v0.2.
