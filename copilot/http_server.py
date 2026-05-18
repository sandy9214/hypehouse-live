"""HTTP JSON-RPC server exposing the copilot's RPC handlers.

PR #53 added the engine-side bridge proxy: every ``library.*`` JSON-RPC
method the UI sends to the engine WS is forwarded as an HTTP POST to
``http://127.0.0.1:8766/rpc`` against this server. Before this module
landed, the copilot had no HTTP listener — the proxy was reaching into
a closed port. This server fills that gap so the engine's proxy hop
completes end-to-end.

Surface:

* ``POST /rpc`` — accepts one JSON-RPC 2.0 request, dispatches to the
  matching handler, returns a JSON-RPC 2.0 response. ``library.*`` is
  routed to :class:`copilot.library_rpc.LibraryRpcHandler`; any future
  namespace can be added via :meth:`JsonRpcHttpServer.register_handler`.
* ``GET /health`` — returns ``{"status": "ok", "service":
  "hypehouse-copilot"}``. Used by the engine for liveness checks before
  it routes proxy traffic.

The server is transport-only. It does not own the library or hold any
domain state — every dispatch goes through the same handler instance
that other transports (Tauri command, in-process tests) call.

Run as a coroutine via :meth:`JsonRpcHttpServer.serve`; the lifecycle
is fully ``await``-driven so callers can ``asyncio.gather`` it with the
engine WS subscriber loop. Bind defaults to ``127.0.0.1:8766``; override
the port via the ``HYPEHOUSE_COPILOT_HTTP_PORT`` env var to match the
engine's ``HYPEHOUSE_COPILOT_URL``.
"""
from __future__ import annotations

import asyncio
import json
import logging
import os
import time
from typing import Any, Awaitable, Callable, Protocol

from aiohttp import web

from .library_rpc import LibraryRpcHandler, RpcError
from .preset_rpc import PresetRpcHandler

log = logging.getLogger(__name__)


# JSON-RPC 2.0 error codes (mirrors docs/api/ws-protocol.md).
JSONRPC_PARSE_ERROR = -32700
JSONRPC_INVALID_REQUEST = -32600
JSONRPC_METHOD_NOT_FOUND = -32601
JSONRPC_INVALID_PARAMS = -32602
JSONRPC_INTERNAL_ERROR = -32603

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 8766
PORT_ENV_VAR = "HYPEHOUSE_COPILOT_HTTP_PORT"


class _Dispatchable(Protocol):
    """Minimal interface every namespace handler must satisfy."""

    def handles(self, method: str) -> bool: ...

    async def dispatch(
        self, method: str, params: dict[str, Any] | None
    ) -> dict[str, Any]: ...


def _error(
    code: int, message: str, *, req_id: object = None, data: object | None = None
) -> dict[str, Any]:
    """Build a JSON-RPC 2.0 error envelope.

    ``req_id`` is ``None`` for parse errors per spec — callers either
    pass the request id (if it was parseable) or omit it.
    """
    err: dict[str, Any] = {"code": int(code), "message": str(message)}
    if data is not None:
        err["data"] = data
    return {"jsonrpc": "2.0", "id": req_id, "error": err}


def _success(result: dict[str, Any], *, req_id: object) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


class JsonRpcHttpServer:
    """aiohttp app hosting the copilot's HTTP RPC surface.

    Owns no domain state — handlers are injected. Lifecycle is
    ``await``-friendly (start / stop coroutines) so the caller can run
    this concurrently with the engine WS subscriber loop via
    :func:`asyncio.gather`.
    """

    SERVICE_NAME = "hypehouse-copilot"

    def __init__(
        self,
        handlers: list[_Dispatchable] | None = None,
        *,
        host: str = DEFAULT_HOST,
        port: int | None = None,
    ):
        self._handlers: list[_Dispatchable] = list(handlers or [])
        self._host = host
        self._port = (
            port
            if port is not None
            else int(os.environ.get(PORT_ENV_VAR, str(DEFAULT_PORT)))
        )
        self._app: web.Application | None = None
        self._runner: web.AppRunner | None = None
        self._site: web.TCPSite | None = None

    # ----- registration ----------------------------------------------

    def register_handler(self, handler: _Dispatchable) -> None:
        """Add a namespace handler (anything implementing the protocol).

        Order matters: handlers are tried in registration order until
        one returns ``True`` from :meth:`handles`. The first match wins.
        """
        self._handlers.append(handler)

    @property
    def port(self) -> int:
        return self._port

    @property
    def host(self) -> str:
        return self._host

    # ----- aiohttp app construction ----------------------------------

    def build_app(self) -> web.Application:
        """Construct (or return cached) the aiohttp ``Application``.

        Exposed so tests can drive the routes through aiohttp's
        ``AioHTTPTestCase`` / ``TestClient`` without binding a TCP socket.
        """
        if self._app is None:
            app = web.Application()
            app.router.add_post("/rpc", self._handle_rpc)
            app.router.add_get("/health", self._handle_health)
            self._app = app
        return self._app

    # ----- lifecycle --------------------------------------------------

    async def start(self) -> None:
        """Bind the TCP socket and start serving. Safe to call once."""
        app = self.build_app()
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        self._site = web.TCPSite(self._runner, self._host, self._port)
        await self._site.start()
        log.info(
            "copilot HTTP RPC server listening on http://%s:%d",
            self._host,
            self._port,
        )

    async def stop(self) -> None:
        """Tear down the TCP socket + runner. Safe to call twice."""
        if self._site is not None:
            await self._site.stop()
            self._site = None
        if self._runner is not None:
            await self._runner.cleanup()
            self._runner = None
        log.info("copilot HTTP RPC server stopped")

    async def serve(self, stop_event: asyncio.Event | None = None) -> None:
        """Start serving + park forever (or until ``stop_event`` is set).

        Convenience wrapper for the common ``asyncio.gather`` pattern
        in :meth:`copilot.service.CoPilotService.run_with_http_server`.
        Caller is responsible for arranging the stop signal — see
        ``copilot/main.py`` for the signal-handler wiring.
        """
        await self.start()
        try:
            if stop_event is None:
                # Park indefinitely; caller cancels the task to stop.
                await asyncio.Event().wait()
            else:
                await stop_event.wait()
        finally:
            await self.stop()

    # ----- request handlers ------------------------------------------

    async def _handle_health(self, _request: web.Request) -> web.Response:
        return web.json_response(
            {"status": "ok", "service": self.SERVICE_NAME}
        )

    async def _handle_rpc(self, request: web.Request) -> web.Response:
        """POST /rpc — parse, dispatch, log, return.

        The HTTP status is always 200 — JSON-RPC carries success/failure
        in the body. Returning a non-200 here would confuse JSON-RPC
        clients that key on ``response.error``.
        """
        # ---- parse body -------------------------------------------------
        raw = await request.read()
        try:
            payload = json.loads(raw.decode("utf-8")) if raw else None
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            log.warning("malformed JSON on /rpc: %s", exc)
            return web.json_response(
                _error(JSONRPC_PARSE_ERROR, "parse error", data=str(exc))
            )

        if not isinstance(payload, dict):
            return web.json_response(
                _error(
                    JSONRPC_INVALID_REQUEST,
                    "request must be a JSON object",
                )
            )

        # ---- envelope validation ---------------------------------------
        req_id = payload.get("id")
        if payload.get("jsonrpc") != "2.0":
            return web.json_response(
                _error(
                    JSONRPC_INVALID_REQUEST,
                    "jsonrpc must be '2.0'",
                    req_id=req_id,
                )
            )
        if "id" not in payload:
            # JSON-RPC 2.0 notifications are valid but we don't expect
            # them here — the engine's proxy always sends a request.
            return web.json_response(
                _error(
                    JSONRPC_INVALID_REQUEST,
                    "missing id (notifications not supported)",
                )
            )
        method = payload.get("method")
        if not isinstance(method, str) or not method:
            return web.json_response(
                _error(
                    JSONRPC_INVALID_REQUEST,
                    "method must be a non-empty string",
                    req_id=req_id,
                )
            )
        params = payload.get("params")
        if params is not None and not isinstance(params, (dict, list)):
            return web.json_response(
                _error(
                    JSONRPC_INVALID_REQUEST,
                    "params must be object/array/omitted",
                    req_id=req_id,
                )
            )
        # JSON-RPC allows positional params (list) but every handler we
        # ship today wants named params; reject list params explicitly
        # rather than silently coerce.
        if isinstance(params, list):
            return web.json_response(
                _error(
                    JSONRPC_INVALID_PARAMS,
                    "positional params not supported; pass an object",
                    req_id=req_id,
                )
            )

        # ---- dispatch ---------------------------------------------------
        handler = self._find_handler(method)
        if handler is None:
            log.info("rpc method=%s -> METHOD_NOT_FOUND", method)
            return web.json_response(
                _error(
                    JSONRPC_METHOD_NOT_FOUND,
                    f"method not found: {method}",
                    req_id=req_id,
                )
            )

        t0 = time.monotonic()
        try:
            result = await handler.dispatch(method, params)
        except RpcError as exc:
            elapsed_ms = (time.monotonic() - t0) * 1000.0
            log.info(
                "rpc method=%s code=%d msg=%r latency_ms=%.2f",
                method,
                exc.code,
                exc.message,
                elapsed_ms,
            )
            return web.json_response(
                _error(exc.code, exc.message, req_id=req_id, data=exc.data)
            )
        except Exception as exc:  # noqa: BLE001 — handler surface is broad
            elapsed_ms = (time.monotonic() - t0) * 1000.0
            log.exception(
                "rpc method=%s INTERNAL_ERROR latency_ms=%.2f",
                method,
                elapsed_ms,
            )
            return web.json_response(
                _error(
                    JSONRPC_INTERNAL_ERROR,
                    f"internal error: {exc}",
                    req_id=req_id,
                )
            )

        elapsed_ms = (time.monotonic() - t0) * 1000.0
        log.info(
            "rpc method=%s ok latency_ms=%.2f", method, elapsed_ms
        )
        return web.json_response(_success(result, req_id=req_id))

    def _find_handler(self, method: str) -> _Dispatchable | None:
        for h in self._handlers:
            if h.handles(method):
                return h
        return None


def build_default_server(
    library_rpc: LibraryRpcHandler,
    *,
    preset_rpc: PresetRpcHandler | None = None,
    host: str = DEFAULT_HOST,
    port: int | None = None,
) -> JsonRpcHttpServer:
    """Construct the production server with the ``library.*`` + ``presets.*`` handlers.

    Kept as a free function so unit tests can spin up a server with the
    same wiring as production without going through :class:`CoPilotService`.
    ``preset_rpc`` defaults to ``None`` for backwards compatibility with
    callers (and tests) that wired only the library handler before — when
    omitted, the ``presets.*`` namespace returns -32601 method not found.
    """
    server = JsonRpcHttpServer(host=host, port=port)
    server.register_handler(library_rpc)
    if preset_rpc is not None:
        server.register_handler(preset_rpc)
    return server


__all__ = [
    "DEFAULT_HOST",
    "DEFAULT_PORT",
    "JSONRPC_INTERNAL_ERROR",
    "JSONRPC_INVALID_PARAMS",
    "JSONRPC_INVALID_REQUEST",
    "JSONRPC_METHOD_NOT_FOUND",
    "JSONRPC_PARSE_ERROR",
    "JsonRpcHttpServer",
    "PORT_ENV_VAR",
    "build_default_server",
]


# The dispatcher protocol uses parameter ``method`` but pylance may
# also resolve via Callable signatures. Keep a tiny adapter type alias
# below for readers who want to wrap an ad-hoc dispatch function
# without subclassing ``_Dispatchable``.
DispatchFn = Callable[[str, dict[str, Any] | None], Awaitable[dict[str, Any]]]
