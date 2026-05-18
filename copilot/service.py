"""JSON-RPC over WebSocket service loop.

Connects to the Rust engine's WebSocket (default ``ws://127.0.0.1:8765``,
configurable via ``HYPEHOUSE_ENGINE_WS``), subscribes to
``engine.state_changed`` notifications, and when a deck nears its end while
co-pilot mode is engaged it:

1. Calls :func:`copilot.decisions.next_track_decision`.
2. Calls :func:`copilot.decisions.transition_plan`.
3. Submits the resulting events to the engine via ``engine.submit_event``.

The loop is intentionally single-threaded asyncio; the only state it carries
is the last-seen engine state + a "decision in flight" guard so the same
state-changed notification doesn't trigger two parallel decisions.
"""
from __future__ import annotations

import asyncio
import json
import logging
import os
import random
from typing import Any, Awaitable, Callable

import aiohttp
from pydantic import ValidationError

from .decisions import NextTrackPlan, next_track_decision, transition_plan
from .engine_client import EngineClient
from .http_server import JsonRpcHttpServer, build_default_server
from .library import TrackLibrary
from .library_rpc import LibraryRpcHandler
from .proposer import Proposal, TransitionProposer
from .schemas import (
    DeckId,
    EngineState,
    Event,
    JsonRpcNotification,
    JsonRpcRequest,
    StateChangedParams,
)

log = logging.getLogger(__name__)

DEFAULT_ENGINE_WS = "ws://127.0.0.1:8765"
# Trigger threshold from ADR-002: co-pilot picks when <30s remain on the
# playing deck.
_END_OF_TRACK_TRIGGER_MS = 30_000

# Reconnect backoff bounds — keep small so an engine restart doesn't leave
# the co-pilot offline for minutes.
_RECONNECT_MIN_S = 0.5
_RECONNECT_MAX_S = 30.0


class CoPilotService:
    """Stateful WebSocket client + decision loop.

    Designed so the network surface and the decision functions are
    independently testable: the test suite injects ``send_request`` /
    ``handle_notification`` directly without standing up a real WS connection.
    """

    def __init__(
        self,
        library: TrackLibrary,
        engine_ws_url: str | None = None,
        *,
        end_of_track_trigger_ms: int = _END_OF_TRACK_TRIGGER_MS,
        bridge_token: str = "",
        proposer: TransitionProposer | None = None,
    ):
        self._library = library
        self._engine_ws_url = engine_ws_url or os.environ.get(
            "HYPEHOUSE_ENGINE_WS", DEFAULT_ENGINE_WS
        )
        self._end_of_track_trigger_ms = end_of_track_trigger_ms
        self._bridge_token = bridge_token or os.environ.get(
            "HYPEHOUSE_BRIDGE_TOKEN", ""
        )

        # Per-deck "decision already submitted for this transition" guards.
        # Cleared when the deck no longer satisfies the trigger condition
        # (typically once the target deck has been loaded + started).
        self._decision_in_flight: set[DeckId] = set()

        # Last-seen state — used by tests + for diagnostics.
        self._last_state: EngineState | None = None

        # Next request id for outbound JSON-RPC requests.
        self._next_id = 1

        # The active websocket — set inside ``run()``.
        self._ws: aiohttp.ClientWebSocketResponse | None = None

        # Proposer for the new run_with_proposer() loop. Created lazily
        # when not injected so unit tests that only exercise
        # handle_notification() don't pay the cost.
        self._proposer: TransitionProposer | None = proposer

        # Library RPC handler — exposes ``library.*`` methods to whatever
        # transport wires them up (UI WS server, future engine proxy).
        # Owned by the service so it shares the library handle + lifetime.
        self._library_rpc = LibraryRpcHandler(library)

    @property
    def library_rpc(self) -> LibraryRpcHandler:
        """Public accessor — transport wiring asks for this to dispatch."""
        return self._library_rpc

    # ----- public surface for tests + callers -----

    @property
    def last_state(self) -> EngineState | None:
        return self._last_state

    async def handle_notification(
        self,
        notification: JsonRpcNotification,
        send_request: Callable[[JsonRpcRequest], Awaitable[None]],
    ) -> None:
        """Process one inbound JSON-RPC notification.

        Public + side-effect-friendly: tests call this directly with a fake
        ``send_request`` callable to assert on the outbound requests.
        """
        if notification.method != "engine.state_changed":
            log.debug("ignoring notification: %s", notification.method)
            return

        try:
            params = StateChangedParams.model_validate(notification.params)
        except ValidationError as exc:
            log.warning("malformed engine.state_changed: %s", exc)
            return

        self._last_state = params.state
        await self._maybe_trigger_decision(params.state, send_request)

    async def _maybe_trigger_decision(
        self,
        state: EngineState,
        send_request: Callable[[JsonRpcRequest], Awaitable[None]],
    ) -> None:
        """Check both decks for the trigger condition and act on it."""
        for deck_id in (DeckId.A, DeckId.B):
            deck = state.deck(deck_id)
            triggered = self._is_trigger_state(deck)
            if not triggered and deck_id in self._decision_in_flight:
                # Clear stale guard once the deck is no longer end-of-track.
                self._decision_in_flight.discard(deck_id)
                continue
            if not triggered:
                continue
            if deck_id in self._decision_in_flight:
                continue
            self._decision_in_flight.add(deck_id)
            try:
                await self._run_decision(state, send_request)
            except Exception:
                # Don't let a single decision blow up the whole service.
                log.exception("decision pipeline raised; releasing guard")
                self._decision_in_flight.discard(deck_id)

    def _is_trigger_state(self, deck) -> bool:  # type: ignore[no-untyped-def]
        if not deck.copilot_engaged:
            return False
        if deck.loaded is None or not deck.playing:
            return False
        track_dur_ms = self._track_duration_ms_for_loaded(deck.loaded.id)
        if track_dur_ms is None:
            return False
        return deck.position_ms > track_dur_ms - self._end_of_track_trigger_ms

    def _track_duration_ms_for_loaded(self, track_id: str) -> int | None:
        ref = self._library.get(track_id)
        if ref is None:
            return None
        return int(ref.duration_s * 1000)

    async def _run_decision(
        self,
        state: EngineState,
        send_request: Callable[[JsonRpcRequest], Awaitable[None]],
    ) -> None:
        plan: NextTrackPlan | None = next_track_decision(state, self._library)
        if plan is None:
            log.info("next_track_decision returned no candidate — staying put")
            return
        events: list[Event] = transition_plan(state, plan)
        log.info(
            "co-pilot picked %s (deck=%s, score=%.3f) — emitting %d events",
            plan.incoming_track.track_id,
            plan.target_deck.value,
            plan.score.total,
            len(events),
        )
        for ev in events:
            req = JsonRpcRequest(
                id=self._alloc_id(),
                method="engine.submit_event",
                params={"event": ev.model_dump(mode="json")},
            )
            await send_request(req)

    def _alloc_id(self) -> int:
        i = self._next_id
        self._next_id += 1
        return i

    # ----- network loop -----

    async def run(self) -> None:
        """Main loop: connect → subscribe → process → reconnect on failure.

        Exponential backoff with jitter caps at ``_RECONNECT_MAX_S``. A clean
        WS close also triggers a reconnect (engine restarts shouldn't leave
        the co-pilot offline).
        """
        backoff = _RECONNECT_MIN_S
        async with aiohttp.ClientSession() as session:
            while True:
                try:
                    log.info("connecting to engine at %s", self._engine_ws_url)
                    async with session.ws_connect(
                        self._engine_ws_url, heartbeat=10.0
                    ) as ws:
                        self._ws = ws
                        backoff = _RECONNECT_MIN_S  # reset on successful connect
                        await self._after_connect(ws)
                        await self._consume(ws)
                except (aiohttp.ClientError, asyncio.TimeoutError, OSError) as exc:
                    log.warning("engine connection lost (%s); reconnecting", exc)
                finally:
                    self._ws = None
                # Backoff w/ jitter.
                jitter = random.uniform(0.0, backoff * 0.25)
                await asyncio.sleep(backoff + jitter)
                backoff = min(backoff * 2.0, _RECONNECT_MAX_S)

    async def _after_connect(self, ws: aiohttp.ClientWebSocketResponse) -> None:
        """Send the initial subscribe RPC after a fresh connection."""
        sub = JsonRpcRequest(
            id=self._alloc_id(),
            method="engine.subscribe",
            params={"topics": ["engine.state_changed"]},
        )
        await ws.send_str(sub.model_dump_json())

    async def _consume(self, ws: aiohttp.ClientWebSocketResponse) -> None:
        async def _send_request(req: JsonRpcRequest) -> None:
            await ws.send_str(req.model_dump_json())

        async for msg in ws:
            if msg.type == aiohttp.WSMsgType.TEXT:
                try:
                    payload: dict[str, Any] = json.loads(msg.data)
                except json.JSONDecodeError:
                    log.warning("non-JSON frame from engine: %r", msg.data[:200])
                    continue
                # Notifications have no "id"; responses to our requests do.
                if "method" in payload and "id" not in payload:
                    try:
                        notif = JsonRpcNotification.model_validate(payload)
                    except ValidationError as exc:
                        log.warning("invalid notification: %s", exc)
                        continue
                    await self.handle_notification(notif, _send_request)
                else:
                    # Response or error — log only for v0.1.
                    log.debug("engine response: %s", payload)
            elif msg.type == aiohttp.WSMsgType.ERROR:
                log.warning("engine WS error: %s", ws.exception())
                break
            elif msg.type in (aiohttp.WSMsgType.CLOSE, aiohttp.WSMsgType.CLOSED):
                log.info("engine WS closed")
                break

    # ----- proposer-based loop (PR #N: copilot-engine-ws-subscribe) -----

    @property
    def proposer(self) -> TransitionProposer:
        """Lazy-init the proposer so tests not exercising it skip the cost."""
        if self._proposer is None:
            self._proposer = TransitionProposer(self._library)
        return self._proposer

    async def run_with_proposer(self) -> None:
        """Modern wiring: :class:`EngineClient` + :class:`TransitionProposer`.

        Differences from the legacy :meth:`run`:

        * Uses the ``websockets`` transport with explicit ``auth.hello``.
        * Funnels state-changed through the proposer's hysteresis filter
          rather than the per-deck "decision in flight" guard.
        * Submits events via ``EngineClient.call("engine.submit_event")``
          and awaits the response — easier to surface engine-side errors
          (e.g. ``-32000 ENGINE_OFFLINE``) than the fire-and-forget
          ``ws.send_str`` legacy path.

        Either ``run()`` *or* ``run_with_proposer()`` should be in flight
        at a time — they share the same proposer / decision bookkeeping.
        """
        client = EngineClient(self._engine_ws_url, token=self._bridge_token)
        proposer = self.proposer

        async def on_state(state: EngineState) -> None:
            self._last_state = state
            proposal: Proposal | None = proposer.on_state(state)
            if proposal is None:
                return
            # Per-deck copilot_engaged gate — proposer doesn't enforce
            # this (it's pure on library compatibility) so we check
            # here. The receiving deck is `target_deck`.
            target_deck = proposal.transition_plan.target_deck
            if not state.deck(target_deck).copilot_engaged:
                # The *current* state's target deck may be co-pilot-off
                # even if the playing deck has co-pilot on. We respect
                # the receiving deck's engagement, not the playing one,
                # to keep the operator's "this deck is mine" toggle
                # authoritative.
                log.debug(
                    "proposer fired for deck %s but copilot_engaged=False; suppressed",
                    target_deck.value,
                )
                return
            log.info(
                "proposer: load %s on deck %s (confidence=%.2f) — %d events",
                proposal.next_track_id,
                target_deck.value,
                proposal.confidence,
                len(proposal.events),
            )
            for ev in proposal.events:
                try:
                    await client.call(
                        "engine.submit_event",
                        {"event": ev.model_dump(mode="json")},
                    )
                except (RuntimeError, asyncio.TimeoutError) as exc:
                    log.warning(
                        "submit_event failed (%s); abandoning rest of plan",
                        exc,
                    )
                    return

        await client.subscribe(on_state)
        try:
            await client.run()
        finally:
            await client.aclose()

    # ----- combined loop: HTTP RPC server + engine WS subscriber -----

    def build_http_server(
        self,
        *,
        host: str | None = None,
        port: int | None = None,
    ) -> JsonRpcHttpServer:
        """Construct the JSON-RPC HTTP server bound to this service's handlers.

        Kept as a method so ``CoPilotService`` owns the wiring (it
        knows which handlers to register). Today only ``library.*`` is
        wired; future namespaces (e.g. ``copilot.*`` for proposer
        introspection) can be added here without touching ``main.py``.
        """
        if host is not None:
            server = JsonRpcHttpServer(host=host, port=port)
            server.register_handler(self._library_rpc)
            return server
        return build_default_server(self._library_rpc, port=port)

    async def run_with_http_server(
        self,
        *,
        http_host: str | None = None,
        http_port: int | None = None,
        use_legacy_engine_loop: bool = False,
    ) -> None:
        """Run the HTTP RPC server AND the engine WS subscriber concurrently.

        The two loops are independent: a failure in one shouldn't take
        the other down silently. ``asyncio.gather(..., return_exceptions=False)``
        propagates the first failure, which is what we want — the
        service-runner in ``main.py`` will log it and exit.

        Caller picks the engine-side loop via ``use_legacy_engine_loop``;
        the default is :meth:`run_with_proposer` (modern path with
        ``EngineClient`` + ``TransitionProposer``).
        """
        server = self.build_http_server(host=http_host, port=http_port)
        engine_coro: Awaitable[None] = (
            self.run() if use_legacy_engine_loop else self.run_with_proposer()
        )
        await server.start()
        try:
            await engine_coro
        finally:
            await server.stop()
