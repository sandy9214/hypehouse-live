"""Async WebSocket client for the Rust engine's JSON-RPC bridge.

Responsibilities (kept narrow on purpose — the decision pipeline lives in
:mod:`copilot.proposer` and :mod:`copilot.decisions`, the wiring lives in
:mod:`copilot.service`):

* Open a WebSocket to the engine.
* Run the in-band ``auth.hello`` handshake (see ``docs/api/ws-protocol.md``).
* Demux response frames against an id-keyed Future map so concurrent
  ``call()`` invocations are correctly paired.
* Demux notification frames to a single registered handler for
  ``engine.state_changed`` (other notifications are dropped — extend the
  dispatch table if the engine adds more).
* Auto-reconnect with exponential backoff (1s → 30s), mirroring the UI
  client's policy.

The transport is :mod:`websockets` (not :mod:`aiohttp` like the legacy
:class:`~copilot.service.CoPilotService`); this keeps a single transport
dependency in the engine-facing layer and frees aiohttp to be optional
later if we want to drop it from the dependency closure.
"""
from __future__ import annotations

import asyncio
import contextlib
import json
import logging
from typing import Any, Awaitable, Callable

import websockets
from pydantic import ValidationError
from websockets.asyncio.client import ClientConnection, connect
from websockets.exceptions import ConnectionClosed

from .schemas import (
    EngineState,
    JsonRpcNotification,
    JsonRpcRequest,
    StateChangedParams,
)

log = logging.getLogger(__name__)


# Reconnect backoff bounds. Exponential, capped at 30s, mirrors the UI WS
# client's policy so an engine restart doesn't leave any client offline
# for minutes.
_RECONNECT_MIN_S = 1.0
_RECONNECT_MAX_S = 30.0

# Default per-call response timeout. Engine RPCs are local and return in
# single-digit ms under normal load; 2s is a generous ceiling that still
# fails-fast on a wedged engine.
DEFAULT_CALL_TIMEOUT_S = 2.0

# How long to wait for ``auth.hello`` to settle. The engine closes
# pending-auth connections after 5s (`PENDING_AUTH_TIMEOUT` in
# ``ws_server.rs``) so we keep our client-side window strictly tighter.
_AUTH_TIMEOUT_S = 4.0


class AuthError(Exception):
    """Raised when ``auth.hello`` is rejected by the engine (-32002).

    Reconnect logic treats this as fatal — retrying the same token is
    pointless. Callers should surface it to the operator.
    """


# Type aliases keep the public surface readable.
StateChangedHandler = Callable[[EngineState], Awaitable[None]]


class EngineClient:
    """Async JSON-RPC client over WebSocket against the Rust engine.

    Lifecycle:

        client = EngineClient("ws://127.0.0.1:8765", token="")
        await client.connect()
        await client.subscribe(on_state)
        result = await client.call("engine.snapshot", {})
        ...
        await client.aclose()

    Or, to run the full auto-reconnect loop, use :meth:`run` from the
    service layer — that's the public surface
    :class:`copilot.service.CoPilotService` consumes.
    """

    def __init__(
        self,
        ws_url: str,
        token: str = "",
        *,
        call_timeout_s: float = DEFAULT_CALL_TIMEOUT_S,
    ) -> None:
        self._ws_url = ws_url
        self._token = token
        self._call_timeout_s = call_timeout_s

        # id → Future for in-flight requests. Each call() allocates a
        # unique int id, sends the frame, and awaits the future.
        self._pending: dict[int | str, asyncio.Future[Any]] = {}
        self._next_id: int = 1

        # The single registered state_changed handler. Multiple handlers
        # are out of scope for v0.1; the proposer fans out if needed.
        self._state_handler: StateChangedHandler | None = None

        # Active connection — owned by run()/connect() and the reader task.
        self._ws: ClientConnection | None = None
        self._reader_task: asyncio.Task[None] | None = None

        # Set after a successful auth.hello, cleared on disconnect.
        self._authed = asyncio.Event()

        # When set, the main loop exits cleanly on next reconnect cycle.
        self._closed = asyncio.Event()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    @property
    def connected(self) -> bool:
        """True iff a WS is open AND ``auth.hello`` has succeeded on it."""
        return self._ws is not None and self._authed.is_set()

    async def connect(self) -> None:
        """Open the WS + perform ``auth.hello``.

        Single-shot variant of :meth:`run` — does NOT auto-reconnect.
        Useful for tests and short-lived diagnostic clients. Raises
        :class:`AuthError` if the token is rejected; propagates the
        underlying ``ConnectionRefusedError``/``OSError`` if the engine
        is unreachable.
        """
        ws = await connect(self._ws_url)
        try:
            await self._handshake(ws)
        except Exception:
            await ws.close()
            raise
        self._ws = ws
        # Spawn the reader so notifications flow to the registered handler.
        self._reader_task = asyncio.create_task(
            self._reader_loop(ws), name="engine-client-reader"
        )

    async def subscribe(self, on_state_changed: StateChangedHandler) -> None:
        """Register a handler for ``engine.state_changed`` notifications.

        The handler is called sequentially (await-ed) for every
        notification — the reader loop does NOT spawn a new task per
        notification. If the handler is slow, notifications back up
        in the WS receive buffer; the proposer is designed to be
        cheap (Python sort over <100 candidates) so this stays well
        within the engine's broadcast cadence.

        The engine broadcasts ``engine.state_changed`` to every authed
        client unconditionally — no explicit ``engine.subscribe`` RPC
        exists today. This method is therefore client-local registration
        only.
        """
        self._state_handler = on_state_changed

    async def call(
        self,
        method: str,
        params: dict[str, Any] | list[Any] | None = None,
        *,
        timeout: float | None = None,
    ) -> Any:
        """Send a JSON-RPC request and await the matching response.

        Raises:
            RuntimeError: if the client is not connected.
            asyncio.TimeoutError: if no response arrives within ``timeout``.
            RuntimeError: if the engine returns an error envelope.
        """
        if self._ws is None:
            raise RuntimeError("EngineClient.call() before connect()")
        # Wait for auth to complete — call() before auth would be a
        # protocol violation (engine returns -32002). We make it a clean
        # local error instead.
        if not self._authed.is_set():
            await asyncio.wait_for(self._authed.wait(), timeout=_AUTH_TIMEOUT_S)

        req_id = self._alloc_id()
        fut: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._pending[req_id] = fut

        req = JsonRpcRequest(
            id=req_id, method=method, params=params if params is not None else {}
        )
        try:
            await self._ws.send(req.model_dump_json())
            return await asyncio.wait_for(
                fut, timeout=timeout if timeout is not None else self._call_timeout_s
            )
        finally:
            self._pending.pop(req_id, None)

    async def aclose(self) -> None:
        """Close the connection and stop any auto-reconnect loop."""
        self._closed.set()
        # Cancel reader first so it doesn't observe the close as an error.
        if self._reader_task is not None and not self._reader_task.done():
            self._reader_task.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await self._reader_task
        if self._ws is not None:
            with contextlib.suppress(Exception):
                await self._ws.close()
        self._ws = None
        self._authed.clear()
        # Cancel any pending callers.
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(ConnectionClosed(None, None))
        self._pending.clear()

    async def run(self) -> None:
        """Connect-forever loop with exponential backoff.

        Yields control between reconnect attempts. Stops cleanly when
        :meth:`aclose` is called. :class:`AuthError` is fatal — it
        propagates out of ``run()`` so the caller can surface to the
        operator instead of looping forever on a bad token.
        """
        backoff = _RECONNECT_MIN_S
        while not self._closed.is_set():
            try:
                log.info("engine_client: connecting to %s", self._ws_url)
                async with connect(self._ws_url) as ws:
                    await self._handshake(ws)
                    self._ws = ws
                    backoff = _RECONNECT_MIN_S
                    await self._reader_loop(ws)
            except AuthError:
                # Fatal — token is wrong, retrying won't help.
                raise
            except (
                ConnectionClosed,
                ConnectionRefusedError,
                OSError,
                asyncio.TimeoutError,
            ) as exc:
                log.warning(
                    "engine_client: connection lost (%s); reconnecting in %.1fs",
                    type(exc).__name__,
                    backoff,
                )
            finally:
                self._ws = None
                self._authed.clear()
            if self._closed.is_set():
                break
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2.0, _RECONNECT_MAX_S)

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _alloc_id(self) -> int:
        i = self._next_id
        self._next_id += 1
        return i

    async def _handshake(self, ws: ClientConnection) -> None:
        """Run ``auth.hello``; raise :class:`AuthError` on rejection.

        When the engine is unauthenticated (no ``HYPEHOUSE_BRIDGE_TOKEN``
        set) the call succeeds against any token. We always send it —
        that's strictly cheaper than feature-detecting.
        """
        req_id = self._alloc_id()
        req = JsonRpcRequest(
            id=req_id, method="auth.hello", params={"token": self._token}
        )
        await ws.send(req.model_dump_json())
        try:
            raw = await asyncio.wait_for(ws.recv(), timeout=_AUTH_TIMEOUT_S)
        except asyncio.TimeoutError as exc:
            raise AuthError("auth.hello timed out") from exc

        if isinstance(raw, bytes):
            raw = raw.decode("utf-8", errors="replace")
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise AuthError(f"auth.hello: non-JSON response: {raw[:80]!r}") from exc
        if payload.get("id") != req_id:
            # Out-of-order frame; engine is supposed to respond first.
            raise AuthError(f"auth.hello: id mismatch (got {payload.get('id')!r})")
        if "error" in payload:
            code = payload["error"].get("code")
            msg = payload["error"].get("message", "auth rejected")
            raise AuthError(f"auth.hello rejected: {code} {msg}")
        # result.authed must be true; older engines just returned `true`,
        # accept both for forward-compat.
        result = payload.get("result")
        ok = result is True or (isinstance(result, dict) and bool(result.get("authed")))
        if not ok:
            raise AuthError(f"auth.hello: unexpected result {result!r}")
        self._authed.set()
        log.info("engine_client: authed (session=%s)", _session_marker(result))

    async def _reader_loop(self, ws: ClientConnection) -> None:
        """Drain incoming frames; dispatch responses + notifications.

        Exits silently on ``ConnectionClosed`` (caller's outer loop
        decides whether to reconnect). Cancellation also exits silently.
        """
        try:
            async for raw in ws:
                if isinstance(raw, bytes):
                    raw = raw.decode("utf-8", errors="replace")
                try:
                    payload = json.loads(raw)
                except json.JSONDecodeError:
                    log.warning("engine_client: non-JSON frame: %r", raw[:200])
                    continue
                # Response (has id, may have result OR error).
                if "id" in payload and (
                    "result" in payload or "error" in payload
                ):
                    self._handle_response(payload)
                    continue
                # Notification (has method, no id).
                if "method" in payload and "id" not in payload:
                    await self._handle_notification(payload)
                    continue
                log.debug("engine_client: unrecognized frame: %s", payload)
        except ConnectionClosed:
            log.debug("engine_client: reader saw close")
        except asyncio.CancelledError:
            raise
        except Exception:  # noqa: BLE001
            log.exception("engine_client: reader loop crashed")

    def _handle_response(self, payload: dict[str, Any]) -> None:
        raw_id = payload.get("id")
        if not isinstance(raw_id, (int, str)):
            log.debug("engine_client: response with non-int/str id %r", raw_id)
            return
        fut = self._pending.get(raw_id)
        if fut is None:
            log.debug("engine_client: response for unknown id %r", raw_id)
            return
        if fut.done():
            return
        if "error" in payload:
            err = payload["error"]
            fut.set_exception(
                RuntimeError(
                    f"engine error {err.get('code')}: {err.get('message')!s}"
                )
            )
        else:
            fut.set_result(payload.get("result"))

    async def _handle_notification(self, payload: dict[str, Any]) -> None:
        try:
            notif = JsonRpcNotification.model_validate(payload)
        except ValidationError as exc:
            log.warning("engine_client: malformed notification: %s", exc)
            return
        if notif.method != "engine.state_changed":
            log.debug("engine_client: dropping notification %s", notif.method)
            return
        if self._state_handler is None:
            return
        try:
            params = StateChangedParams.model_validate(notif.params)
        except ValidationError as exc:
            log.warning("engine_client: malformed state_changed: %s", exc)
            return
        try:
            await self._state_handler(params.state)
        except Exception:  # noqa: BLE001
            # Never let a handler raise propagate into the reader loop —
            # that would tear the connection down on a transient bug.
            log.exception("engine_client: state_changed handler raised")


def _session_marker(result: Any) -> str:
    if isinstance(result, dict):
        s = result.get("session")
        if s is not None:
            return str(s)
    return "?"


__all__ = [
    "AuthError",
    "DEFAULT_CALL_TIMEOUT_S",
    "EngineClient",
    "StateChangedHandler",
]

# Re-export for convenience — service.py imports these symbols.
_ = websockets  # keep import live for tooling
