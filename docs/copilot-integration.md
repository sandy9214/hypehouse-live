# Co-pilot ↔ engine integration

**Status**: v0.1 — first end-to-end wiring (PR `copilot-engine-ws-subscribe`).
**Owners**: `copilot/` Python service ↔ `engine/src/bridge/` Rust WS server.

This doc explains how the Python co-pilot connects to the Rust engine,
authenticates, observes state changes, and proposes the next track +
transition plan. It is the contract spec for the modules below; the
JSON-RPC frame shapes live in [`docs/api/ws-protocol.md`](./api/ws-protocol.md).

## Module map

```
copilot/
├── engine_client.py    # ← NEW (this PR). WebSocket transport + auth.hello + reconnect.
├── proposer.py         # ← NEW (this PR). Mashability ranker + hysteresis + Proposal shape.
├── service.py          # CoPilotService — wires EngineClient + TransitionProposer.
├── decisions.py        # Pure decision functions (next_track_decision, transition_plan).
├── library.py          # SQLite-backed TrackLibrary (read-only at runtime).
└── schemas.py          # Pydantic mirrors of engine/src/state.rs serde shapes.
```

## End-to-end flow

```
            engine_client.run()
                │
                ▼
   ┌────────────────────────────┐
   │  open ws://engine          │
   │  send auth.hello {token}   │ ──── -32002 ────► AuthError (fatal, no retry)
   │  await success             │
   └─────────────┬──────────────┘
                 │
                 ▼
   ┌────────────────────────────┐         engine pushes
   │  reader loop                │ ◄────  engine.state_changed
   │   • route responses by id   │         (broadcast to every authed client)
   │   • dispatch state_changed  │
   │     → TransitionProposer    │
   └─────────────┬──────────────┘
                 │
                 ▼
   ┌────────────────────────────┐
   │ TransitionProposer.on_state │
   │   • run next_track_decision │
   │   • check hysteresis (8 bts)│
   │   • build Proposal          │
   └─────────────┬──────────────┘
                 │  Proposal { next_track_id, transition_plan, confidence, events }
                 ▼
   ┌────────────────────────────┐
   │ service: per-deck gate      │
   │   • copilot_engaged?        │
   │   • submit_event × N        │ ──── -32000 ────► log + abandon plan
   └────────────────────────────┘
```

## EngineClient contract

`EngineClient(ws_url, token, *, call_timeout_s=2.0)`:

* `await connect()` — single-shot. Opens WS, runs `auth.hello`, spawns
  reader task. Raises `AuthError` on `-32002`; raises
  `ConnectionRefusedError` / `OSError` on transport failure.
* `await subscribe(on_state_changed)` — registers an async handler for
  `engine.state_changed`. The handler is called sequentially from the
  reader loop; no parallel fan-out.
* `await call(method, params, *, timeout=None)` — sends a JSON-RPC
  request, awaits the matching response. Concurrent calls are routed
  by id. Raises `asyncio.TimeoutError` on no response; `RuntimeError`
  on engine error envelope.
* `await run()` — connect-forever loop with exponential backoff
  (1s → 30s). Re-runs `auth.hello` on every reconnect. Auth failure
  is fatal and propagates out.
* `await aclose()` — cancel reader, close socket, idempotent.

Notes:
* Transport is the [`websockets`](https://websockets.readthedocs.io/) library.
* The legacy `CoPilotService.run()` (aiohttp) is kept for back-compat;
  new code should use `CoPilotService.run_with_proposer()`.
* The engine has **no `engine.subscribe` RPC** — `state_changed` is
  auto-broadcast to every authed client. `subscribe()` is therefore
  client-local handler registration only.

## TransitionProposer contract

`TransitionProposer(library, *, hysteresis_beats=8, transition_bars=16)`:

* `on_state(state) -> Proposal | None` — pure synchronous function.
  Returns `None` if no deck is playing, no library candidate passes
  the gates, or the hysteresis window hasn't elapsed.
* `reset()` — clears hysteresis bookkeeping (call after reconnect).

`Proposal`:

```python
@dataclass(frozen=True)
class Proposal:
    next_track_id: str
    transition_plan: TransitionPlanShape  # target_deck + crossfader ramp + EQ swap timing
    confidence: float                      # 0..1, monotone-decreasing in penalty
    events: tuple[Event, ...]              # ready for engine.submit_event
    score: MashabilityFactors
```

`TransitionPlanShape`:

```python
@dataclass(frozen=True)
class TransitionPlanShape:
    target_deck: DeckId                    # A or B
    crossfader_from: float                 # 0.0 = A audible, 1.0 = B audible
    crossfader_to: float
    crossfader_ramp_duration_ms: int       # 16 bars × 4 beats × beat_period_ms
    eq_swap_at_ms: int                     # midpoint of the ramp (v0.1)
    beat_align_at_ms: int                  # 0 today; engine resolves "next downbeat"
```

### Hysteresis policy

`state_changed` fires on every accepted event — during a transition
that can be dozens per second. The proposer suppresses re-proposals
that pick the **same track** for the **same target deck** within
`hysteresis_beats × beat_period` wall-clock seconds (default 8 beats,
≈ 3.9s at 124 BPM).

A re-proposal with a **different** track is **not** suppressed — that
catches "the operator just added a better donor to the library" cases.

The clock is `time.monotonic` (or an injectable test stub), not
`state.position_ms`, because position resets to 0 on every track load
and would otherwise yield false "we just proposed" outcomes after a
swap.

## CLI

```bash
hypehouse-copilot \
  --engine-url ws://127.0.0.1:8765 \   # alias: --engine-ws
  --bridge-token "$HYPEHOUSE_BRIDGE_TOKEN" \
  --library-db ~/.hypehouse-live/library.db
```

Default is the new proposer-based loop. Pass `--legacy-loop` to use the
original aiohttp `CoPilotService.run()` path (kept while the new path
beds in; will be removed after one release).

## Test coverage

| File | Coverage |
|------|----------|
| `tests/test_engine_client.py` | auth.hello, id-keyed call/response, subscribe dispatch, auto-reconnect, AuthError on -32002 |
| `tests/test_proposer.py` | top-ranked candidate, hysteresis cooldown, library-change bypass, reset, empty-library fallthrough |
| `tests/test_e2e_proposal.py` | end-to-end against real `hypehouse-engine` binary (skipped in CI; local only) |
| `tests/test_service_integration.py` | existing aiohttp `handle_notification` + reconnect path (untouched) |

## Open questions / follow-up PRs

* **Stem-aware EQ swap** — v0.1 fires the EQ kill at ramp midpoint
  unconditionally. v0.2 should compute it from the outgoing track's
  last-chorus end via the vendored analyzer's section detection.
* **Phase-aligned start** — `beat_align_at_ms=0` today; engine resolves
  "next downbeat" lazily. Once the engine ships ADR-007 clock-sync,
  the proposer should pass the absolute target frame.
* **Per-deck token scope** — both decks share one EngineClient. If we
  ever ship per-deck auth (e.g. mobile remote controlling just deck B),
  this will need to fan out.
* **Backpressure on submit_event** — current code abandons the
  remaining events on `-32000`. A retry-with-backoff might be safer
  for transient channel-full conditions.
