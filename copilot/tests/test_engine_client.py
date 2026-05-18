"""EngineClient tests against a mock ``websockets`` server.

We don't go through the real Rust engine here — that's covered by the
end-to-end test (``test_e2e_proposal.py``). The contract these tests
pin down:

* ``auth.hello`` is the first frame the client sends; it carries the
  configured token and waits for ``{"authed": true, "session": ...}``.
* Concurrent ``call()``s get distinct ids and distinct response futures.
* The subscribe handler is invoked when the server pushes
  ``engine.state_changed``.
* Auto-reconnect re-runs ``auth.hello`` on every fresh connection.
* Invalid token → :class:`AuthError`.
"""
from __future__ import annotations

import asyncio
import json
import socket
from typing import Any

import pytest
from websockets.asyncio.server import ServerConnection, serve

from copilot.engine_client import AuthError, EngineClient
from copilot.schemas import (
    Deck,
    EngineState,
    StateChangedParams,
    TrackRef as EngineTrackRef,
)


pytestmark = pytest.mark.asyncio


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


async def _read_one(ws: ServerConnection) -> dict[str, Any]:
    raw = await asyncio.wait_for(ws.recv(), timeout=5.0)
    if isinstance(raw, bytes):
        raw = raw.decode("utf-8")
    return json.loads(raw)


async def _accept_auth(ws: ServerConnection, *, valid_token: str = "") -> bool:
    """Read a frame, expect ``auth.hello``, respond appropriately.

    Returns True if auth succeeded (test can continue using the socket).
    """
    msg = await _read_one(ws)
    assert msg.get("method") == "auth.hello", f"expected auth.hello got {msg!r}"
    token = msg.get("params", {}).get("token", "")
    if token == valid_token:
        await ws.send(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": {"authed": True, "session": 1734567890123456},
                }
            )
        )
        return True
    await ws.send(
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {
                    "code": -32002,
                    "message": "Authentication rejected",
                    "data": "invalid token",
                },
            }
        )
    )
    return False


def _make_trigger_state() -> EngineState:
    return EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="t1", path="/t1.mp3"),
            playing=True,
            position_ms=1000,
            copilot_engaged=True,
            bpm=124.0,
        ),
        deck_b=Deck(),
        session_active=True,
    )


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


async def test_connect_and_auth_hello() -> None:
    """Client opens a WS, sends auth.hello, server replies success → client
    transitions to ``authed`` and ``connect()`` returns cleanly."""
    port = _free_port()
    received: list[dict[str, Any]] = []

    async def handler(ws: ServerConnection) -> None:
        msg = await _read_one(ws)
        received.append(msg)
        await ws.send(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": {"authed": True, "session": 1734567890123456},
                }
            )
        )
        # Keep the conn open until the client tears down so the reader
        # task doesn't observe a premature close as an error.
        try:
            await asyncio.wait_for(ws.wait_closed(), timeout=5.0)
        except asyncio.TimeoutError:
            pass

    async with serve(handler, "127.0.0.1", port):
        client = EngineClient(f"ws://127.0.0.1:{port}", token="hunter2")
        await client.connect()
        try:
            assert client.connected, "client should be authed after connect()"
        finally:
            await client.aclose()

    assert len(received) == 1, f"expected exactly one frame, got {received}"
    msg = received[0]
    assert msg["method"] == "auth.hello"
    assert msg["params"] == {"token": "hunter2"}
    assert "id" in msg


async def test_call_response_id_pairing() -> None:
    """Two concurrent calls must each get their own response."""
    port = _free_port()

    async def handler(ws: ServerConnection) -> None:
        await _accept_auth(ws)
        # Echo every subsequent request back as a result keyed by id.
        # Reply in REVERSE order to prove the client routes by id, not
        # by arrival order.
        first = await _read_one(ws)
        second = await _read_one(ws)
        await ws.send(
            json.dumps(
                {"jsonrpc": "2.0", "id": second["id"], "result": {"method": second["method"]}}
            )
        )
        await ws.send(
            json.dumps(
                {"jsonrpc": "2.0", "id": first["id"], "result": {"method": first["method"]}}
            )
        )
        try:
            await asyncio.wait_for(ws.wait_closed(), timeout=5.0)
        except asyncio.TimeoutError:
            pass

    async with serve(handler, "127.0.0.1", port):
        client = EngineClient(f"ws://127.0.0.1:{port}")
        await client.connect()
        try:
            results = await asyncio.gather(
                client.call("engine.snapshot", {}),
                client.call("engine.health", {}),
            )
        finally:
            await client.aclose()

    methods = {r["method"] for r in results}
    assert methods == {"engine.snapshot", "engine.health"}


async def test_subscribe_receives_state_changed() -> None:
    """Server pushes a state_changed; client invokes the registered handler."""
    port = _free_port()
    handler_calls: list[EngineState] = []

    async def server_handler(ws: ServerConnection) -> None:
        await _accept_auth(ws)
        await ws.send(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "method": "engine.state_changed",
                    "params": StateChangedParams(state=_make_trigger_state()).model_dump(
                        mode="json"
                    ),
                }
            )
        )
        try:
            await asyncio.wait_for(ws.wait_closed(), timeout=5.0)
        except asyncio.TimeoutError:
            pass

    async def on_state(state: EngineState) -> None:
        handler_calls.append(state)

    async with serve(server_handler, "127.0.0.1", port):
        client = EngineClient(f"ws://127.0.0.1:{port}")
        await client.subscribe(on_state)
        await client.connect()
        try:
            # Allow the reader loop to consume the pushed frame.
            for _ in range(20):
                if handler_calls:
                    break
                await asyncio.sleep(0.05)
        finally:
            await client.aclose()

    assert handler_calls, "state_changed handler was never invoked"
    s = handler_calls[0]
    assert s.deck_a.loaded is not None
    assert s.deck_a.loaded.id == "t1"


async def test_auto_reconnect_on_close() -> None:
    """First connection closes; client must reconnect + re-auth."""
    port = _free_port()
    connect_count = 0
    connect_event = asyncio.Event()

    async def server_handler(ws: ServerConnection) -> None:
        nonlocal connect_count
        connect_count += 1
        connect_event.set()
        # Accept auth then immediately close the socket on the first
        # connection. Second connection stays open until the test ends.
        await _accept_auth(ws)
        if connect_count == 1:
            await ws.close(code=1000, reason="bye")
            return
        try:
            await asyncio.wait_for(ws.wait_closed(), timeout=10.0)
        except asyncio.TimeoutError:
            pass

    async with serve(server_handler, "127.0.0.1", port):
        client = EngineClient(f"ws://127.0.0.1:{port}")
        run_task = asyncio.create_task(client.run(), name="client-run")
        try:
            # Wait for both connection attempts. Allow up to 5s for the
            # exponential-backoff retry (first window = 1s).
            for _ in range(50):
                if connect_count >= 2:
                    break
                await asyncio.sleep(0.1)
            assert connect_count >= 2, (
                f"expected >=2 connects, got {connect_count}"
            )
        finally:
            await client.aclose()
            run_task.cancel()
            try:
                await run_task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass


async def test_invalid_token_raises() -> None:
    """auth.hello -32002 must surface as :class:`AuthError`."""
    port = _free_port()

    async def handler(ws: ServerConnection) -> None:
        await _accept_auth(ws, valid_token="correct")
        # Server hangs around so the client's connection-close error
        # path doesn't fire; we want the AuthError, not a ConnectionError.

    async with serve(handler, "127.0.0.1", port):
        client = EngineClient(f"ws://127.0.0.1:{port}", token="wrong")
        with pytest.raises(AuthError):
            await client.connect()


async def test_call_before_connect_raises() -> None:
    """Defensive: calling ``call()`` before ``connect()`` must raise."""
    client = EngineClient("ws://127.0.0.1:1")  # unreachable, but we never go
    with pytest.raises(RuntimeError):
        await client.call("engine.snapshot", {})


async def test_aclose_is_idempotent() -> None:
    """Calling ``aclose()`` twice is safe — no double-cancel exception."""
    port = _free_port()

    async def handler(ws: ServerConnection) -> None:
        await _accept_auth(ws)
        try:
            await asyncio.wait_for(ws.wait_closed(), timeout=5.0)
        except asyncio.TimeoutError:
            pass

    async with serve(handler, "127.0.0.1", port):
        client = EngineClient(f"ws://127.0.0.1:{port}")
        await client.connect()
        await client.aclose()
        await client.aclose()  # second close must be a no-op
