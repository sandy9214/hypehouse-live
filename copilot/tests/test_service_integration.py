"""Service-loop integration: mock engine WS, simulate ``state_changed``,
assert the service emits the expected ``engine.submit_event`` request.

Two layers:

1. ``test_handle_notification_emits_submit_event_requests`` — bypasses the
   network entirely by calling ``CoPilotService.handle_notification`` with a
   stub ``send_request``. Fast, deterministic, runs in <50 ms.
2. ``test_run_loop_connects_subscribes_and_handles_state_changed`` — spins
   up an aiohttp WS server, points the service at it, drives one full
   subscribe-then-state-changed round trip, and asserts the inbound request
   shape. This is the contract test the engine team will read when wiring
   the Rust side.
"""
from __future__ import annotations

import asyncio
import json

import aiohttp
import pytest
from aiohttp import web

from copilot.library import TrackLibrary, TrackRef
from copilot.schemas import (
    Deck,
    EngineState,
    JsonRpcNotification,
    JsonRpcRequest,
    StateChangedParams,
    TrackRef as EngineTrackRef,
)
from copilot.service import CoPilotService


# -------- Shared helpers --------


def _seed_library(lib: TrackLibrary) -> None:
    lib.add_track(TrackRef("playing", "/playing.mp3", 124.0, "8B", 0.20, 210.0))
    lib.add_track(TrackRef("incoming", "/incoming.mp3", 125.0, "8B", 0.22, 220.0))


def _trigger_state() -> EngineState:
    """A state where deck A is playing 'playing' near its end with co-pilot on."""
    return EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="playing", path="/playing.mp3"),
            playing=True,
            position_ms=190_000,  # 190s of a 210s track
            copilot_engaged=True,
            bpm=124.0,
        ),
        deck_b=Deck(),
        session_active=True,
    )


# -------- Layer 1: pure handle_notification --------


async def test_handle_notification_emits_submit_event_requests():
    lib = TrackLibrary(":memory:")
    try:
        _seed_library(lib)
        service = CoPilotService(lib)

        sent: list[JsonRpcRequest] = []

        async def stub_send(req: JsonRpcRequest) -> None:
            sent.append(req)

        notif = JsonRpcNotification(
            method="engine.state_changed",
            params=StateChangedParams(state=_trigger_state()).model_dump(mode="json"),
        )

        await service.handle_notification(notif, stub_send)

        # All emitted requests are submit_event RPCs.
        assert sent, "service emitted no requests"
        assert all(r.method == "engine.submit_event" for r in sent)
        # Each carries an event in params.
        for r in sent:
            assert "event" in r.params
            assert r.params["event"]["source"] == "Copilot"
        # The first event is DeckLoad of the incoming track.
        first_event = sent[0].params["event"]
        assert first_event["kind"]["kind"] == "DeckLoad"
        assert first_event["kind"]["track"]["id"] == "incoming"
    finally:
        lib.close()


async def test_handle_notification_does_not_fire_when_not_near_end():
    """Position well before the end → no decision."""
    lib = TrackLibrary(":memory:")
    try:
        _seed_library(lib)
        service = CoPilotService(lib)
        sent: list[JsonRpcRequest] = []

        async def stub_send(req: JsonRpcRequest) -> None:
            sent.append(req)

        state = _trigger_state()
        state.deck_a.position_ms = 5_000  # 5s in — nowhere near end
        notif = JsonRpcNotification(
            method="engine.state_changed",
            params=StateChangedParams(state=state).model_dump(mode="json"),
        )
        await service.handle_notification(notif, stub_send)
        assert sent == []
    finally:
        lib.close()


async def test_handle_notification_does_not_fire_when_copilot_disengaged():
    lib = TrackLibrary(":memory:")
    try:
        _seed_library(lib)
        service = CoPilotService(lib)
        sent: list[JsonRpcRequest] = []

        async def stub_send(req: JsonRpcRequest) -> None:
            sent.append(req)

        state = _trigger_state()
        state.deck_a.copilot_engaged = False
        notif = JsonRpcNotification(
            method="engine.state_changed",
            params=StateChangedParams(state=state).model_dump(mode="json"),
        )
        await service.handle_notification(notif, stub_send)
        assert sent == []
    finally:
        lib.close()


async def test_handle_notification_deduplicates_within_window():
    """Two state_changed in a row for the same trigger state must only fire
    one decision pipeline (the ``_decision_in_flight`` guard).
    """
    lib = TrackLibrary(":memory:")
    try:
        _seed_library(lib)
        service = CoPilotService(lib)
        sent: list[JsonRpcRequest] = []

        async def stub_send(req: JsonRpcRequest) -> None:
            sent.append(req)

        notif = JsonRpcNotification(
            method="engine.state_changed",
            params=StateChangedParams(state=_trigger_state()).model_dump(mode="json"),
        )
        await service.handle_notification(notif, stub_send)
        first_round = list(sent)
        sent.clear()
        await service.handle_notification(notif, stub_send)
        assert sent == [], "second identical notification must not re-fire"
        assert first_round, "first notification must have fired"
    finally:
        lib.close()


async def test_handle_notification_ignores_malformed_payload():
    lib = TrackLibrary(":memory:")
    try:
        service = CoPilotService(lib)
        sent: list[JsonRpcRequest] = []

        async def stub_send(req: JsonRpcRequest) -> None:
            sent.append(req)

        # Wrong shape for StateChangedParams.
        notif = JsonRpcNotification(
            method="engine.state_changed",
            params={"this": "is", "not": "valid"},
        )
        await service.handle_notification(notif, stub_send)
        assert sent == []
    finally:
        lib.close()


# -------- Layer 2: real WS round trip against a mock server --------


async def test_run_loop_connects_subscribes_and_handles_state_changed(
    unused_tcp_port: int,
):
    """Spin up an aiohttp WS server playing the engine's role.

    Flow:
      1. Service connects and sends ``engine.subscribe`` (first inbound msg).
      2. Mock engine pushes a ``state_changed`` notification.
      3. Service responds with one+ ``engine.submit_event`` requests.
      4. We close the WS and cancel the service task.
    """
    received_requests: list[dict] = []
    subscribe_seen = asyncio.Event()
    first_submit_seen = asyncio.Event()

    async def ws_handler(request: web.Request) -> web.WebSocketResponse:
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        async for msg in ws:
            if msg.type != aiohttp.WSMsgType.TEXT:
                continue
            payload = json.loads(msg.data)
            received_requests.append(payload)
            if payload.get("method") == "engine.subscribe":
                subscribe_seen.set()
                # Acknowledge.
                ack = {"jsonrpc": "2.0", "id": payload["id"], "result": True}
                await ws.send_str(json.dumps(ack))
                # Push a state_changed notification that should trigger a
                # decision.
                state = _trigger_state()
                notif = {
                    "jsonrpc": "2.0",
                    "method": "engine.state_changed",
                    "params": StateChangedParams(state=state).model_dump(mode="json"),
                }
                await ws.send_str(json.dumps(notif))
            elif payload.get("method") == "engine.submit_event":
                first_submit_seen.set()
        return ws

    app = web.Application()
    app.router.add_get("/", ws_handler)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", unused_tcp_port)
    await site.start()

    lib = TrackLibrary(":memory:")
    _seed_library(lib)
    service = CoPilotService(lib, engine_ws_url=f"ws://127.0.0.1:{unused_tcp_port}/")

    task = asyncio.create_task(service.run(), name="service-run")
    try:
        # The mock server pushes state_changed immediately after subscribe;
        # service should respond with submit_event. Bounded wait keeps the
        # test deterministic.
        await asyncio.wait_for(subscribe_seen.wait(), timeout=5.0)
        await asyncio.wait_for(first_submit_seen.wait(), timeout=5.0)
    finally:
        task.cancel()
        try:
            await task
        except (asyncio.CancelledError, Exception):  # noqa: BLE001
            pass
        await runner.cleanup()
        lib.close()

    # Assertions on the observed wire traffic.
    methods = [r.get("method") for r in received_requests]
    assert "engine.subscribe" in methods
    assert "engine.submit_event" in methods
    # The submit_event payloads are well-formed.
    submits = [r for r in received_requests if r.get("method") == "engine.submit_event"]
    assert submits
    first = submits[0]
    assert first["jsonrpc"] == "2.0"
    assert "id" in first
    assert "event" in first["params"]
    assert first["params"]["event"]["source"] == "Copilot"


@pytest.fixture
def unused_tcp_port() -> int:
    """Lightweight version of pytest-asyncio's ``unused_tcp_port`` so we don't
    pull another plugin."""
    import socket

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


async def test_run_loop_reconnects_after_engine_drop(unused_tcp_port: int):
    """First connection accepted then immediately closed → service must
    reconnect and re-subscribe. We accept two subscriptions in a row to
    prove the reconnect path runs."""
    subscribes = asyncio.Queue()

    async def ws_handler(request: web.Request) -> web.WebSocketResponse:
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        # Read first message (should be subscribe) then close.
        msg = await ws.receive(timeout=5.0)
        if msg.type == aiohttp.WSMsgType.TEXT:
            await subscribes.put(json.loads(msg.data))
        await ws.close()
        return ws

    app = web.Application()
    app.router.add_get("/", ws_handler)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", unused_tcp_port)
    await site.start()

    lib = TrackLibrary(":memory:")
    service = CoPilotService(lib, engine_ws_url=f"ws://127.0.0.1:{unused_tcp_port}/")
    task = asyncio.create_task(service.run(), name="service-run-reconnect")
    try:
        first = await asyncio.wait_for(subscribes.get(), timeout=5.0)
        second = await asyncio.wait_for(subscribes.get(), timeout=10.0)
        assert first.get("method") == "engine.subscribe"
        assert second.get("method") == "engine.subscribe"
    finally:
        task.cancel()
        try:
            await task
        except (asyncio.CancelledError, Exception):  # noqa: BLE001
            pass
        await runner.cleanup()
        lib.close()
