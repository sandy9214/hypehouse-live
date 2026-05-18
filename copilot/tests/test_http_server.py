"""HTTP JSON-RPC server tests.

Drives :class:`copilot.http_server.JsonRpcHttpServer` through aiohttp's
``TestClient`` so we exercise the full request/response cycle (parse,
envelope-validate, dispatch, error-map) without binding a real TCP
socket. ``LibraryRpcHandler`` is wired with an in-memory
``TrackLibrary`` from the shared ``library`` fixture.

The test count goal (from the PR brief) is 8+. Coverage:

* /health 200
* /rpc library.list_tracks happy path
* /rpc library.set_hot_cues round-trip
* /rpc library.list_tracks paginates
* /rpc parse error (malformed JSON)
* /rpc invalid request (missing jsonrpc field)
* /rpc invalid request (missing id field)
* /rpc method not found
* /rpc invalid params surfaced from handler (-32602)
* /rpc 5 concurrent requests don't conflict
* port override via env var
* /rpc positional params rejected
"""
from __future__ import annotations

import asyncio
from typing import Any

import pytest
from aiohttp.test_utils import TestClient, TestServer

from copilot.http_server import (
    DEFAULT_PORT,
    JSONRPC_INTERNAL_ERROR,
    JSONRPC_INVALID_PARAMS,
    JSONRPC_INVALID_REQUEST,
    JSONRPC_METHOD_NOT_FOUND,
    JSONRPC_PARSE_ERROR,
    PORT_ENV_VAR,
    JsonRpcHttpServer,
    build_default_server,
)
from copilot.library import TrackLibrary, TrackRef
from copilot.library_rpc import LibraryRpcHandler


def _seed(lib: TrackLibrary) -> None:
    rows = [
        TrackRef("alpha", "/m/alpha.mp3", 120.0, "8B", 0.2, 200.0),
        TrackRef("bravo", "/m/bravo.mp3", 124.0, "8B", 0.3, 210.0),
        TrackRef("charlie", "/m/charlie.mp3", 128.0, "9B", 0.4, 220.0),
    ]
    for r in rows:
        lib.add_track(r)


def _rpc(method: str, params: dict[str, Any] | None = None, req_id: int = 1) -> dict[str, Any]:
    body: dict[str, Any] = {"jsonrpc": "2.0", "id": req_id, "method": method}
    if params is not None:
        body["params"] = params
    return body


@pytest.fixture
async def client(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    server = JsonRpcHttpServer()
    server.register_handler(handler)
    app = server.build_app()
    async with TestClient(TestServer(app)) as cli:
        yield cli


# ----- /health -------------------------------------------------------


async def test_health_returns_ok(client: TestClient):
    resp = await client.get("/health")
    assert resp.status == 200
    body = await resp.json()
    assert body == {"status": "ok", "service": "hypehouse-copilot"}


# ----- /rpc happy paths ---------------------------------------------


async def test_rpc_library_list_tracks_returns_seeded_rows(client: TestClient):
    resp = await client.post("/rpc", json=_rpc("library.list_tracks", {}))
    assert resp.status == 200
    body = await resp.json()
    assert body["jsonrpc"] == "2.0"
    assert body["id"] == 1
    assert "error" not in body
    result = body["result"]
    assert result["total"] == 3
    ids = [t["id"] for t in result["tracks"]]
    assert ids == ["alpha", "bravo", "charlie"]


async def test_rpc_library_set_hot_cues_round_trip(client: TestClient):
    cues = [0, 1000, None, 4000, None, None, None, None]
    resp = await client.post(
        "/rpc",
        json=_rpc(
            "library.set_hot_cues",
            {"track_id": "bravo", "hot_cues": cues},
        ),
    )
    assert resp.status == 200
    body = await resp.json()
    assert "error" not in body, body
    track = body["result"]["track"]
    assert track["id"] == "bravo"
    assert track["hot_cues"] == cues


async def test_rpc_library_list_tracks_pagination(client: TestClient):
    resp = await client.post(
        "/rpc",
        json=_rpc("library.list_tracks", {"limit": 2, "offset": 1}),
    )
    body = await resp.json()
    assert "error" not in body, body
    ids = [t["id"] for t in body["result"]["tracks"]]
    assert ids == ["bravo", "charlie"]
    assert body["result"]["total"] == 3


# ----- /rpc error paths ---------------------------------------------


async def test_rpc_bad_json_returns_parse_error(client: TestClient):
    resp = await client.post(
        "/rpc",
        data=b"this is not json{",
        headers={"Content-Type": "application/json"},
    )
    assert resp.status == 200  # JSON-RPC carries failure in body
    body = await resp.json()
    assert body["error"]["code"] == JSONRPC_PARSE_ERROR
    # Per JSON-RPC spec, id is null on parse error.
    assert body["id"] is None


async def test_rpc_missing_jsonrpc_field_returns_invalid_request(client: TestClient):
    resp = await client.post(
        "/rpc",
        json={"id": 1, "method": "library.list_tracks"},
    )
    body = await resp.json()
    assert body["error"]["code"] == JSONRPC_INVALID_REQUEST


async def test_rpc_missing_id_returns_invalid_request(client: TestClient):
    resp = await client.post(
        "/rpc",
        json={"jsonrpc": "2.0", "method": "library.list_tracks"},
    )
    body = await resp.json()
    assert body["error"]["code"] == JSONRPC_INVALID_REQUEST


async def test_rpc_unknown_method_returns_method_not_found(client: TestClient):
    resp = await client.post("/rpc", json=_rpc("library.does_not_exist"))
    body = await resp.json()
    assert body["error"]["code"] == JSONRPC_METHOD_NOT_FOUND
    assert "library.does_not_exist" in body["error"]["message"]
    assert body["id"] == 1


async def test_rpc_handler_invalid_params_surface_as_minus_32602(client: TestClient):
    resp = await client.post(
        "/rpc",
        json=_rpc(
            "library.set_hot_cues",
            {"track_id": "bravo", "hot_cues": "not-a-list"},
        ),
    )
    body = await resp.json()
    assert body["error"]["code"] == JSONRPC_INVALID_PARAMS


async def test_rpc_positional_params_rejected(client: TestClient):
    resp = await client.post(
        "/rpc",
        json={"jsonrpc": "2.0", "id": 1, "method": "library.list_tracks", "params": [1, 2]},
    )
    body = await resp.json()
    assert body["error"]["code"] == JSONRPC_INVALID_PARAMS


async def test_rpc_non_object_body_rejected(client: TestClient):
    resp = await client.post(
        "/rpc",
        data=b"[1, 2, 3]",
        headers={"Content-Type": "application/json"},
    )
    body = await resp.json()
    assert body["error"]["code"] == JSONRPC_INVALID_REQUEST


# ----- concurrency ---------------------------------------------------


async def test_rpc_concurrent_requests_do_not_conflict(client: TestClient):
    async def one(rid: int) -> dict[str, Any]:
        resp = await client.post(
            "/rpc", json=_rpc("library.list_tracks", {}, req_id=rid)
        )
        return await resp.json()

    results = await asyncio.gather(*(one(i) for i in range(5)))
    assert {r["id"] for r in results} == {0, 1, 2, 3, 4}
    for r in results:
        assert "error" not in r
        assert r["result"]["total"] == 3


# ----- internal-error path ------------------------------------------


async def test_rpc_handler_internal_error_returns_minus_32603(library: TrackLibrary):
    """A handler raising an unexpected exception collapses to -32603."""

    class BoomHandler:
        def handles(self, method: str) -> bool:
            return method == "boom.go"

        async def dispatch(self, method: str, params):
            raise RuntimeError("kaboom")

    server = JsonRpcHttpServer()
    server.register_handler(BoomHandler())
    app = server.build_app()
    async with TestClient(TestServer(app)) as cli:
        resp = await cli.post("/rpc", json=_rpc("boom.go"))
        body = await resp.json()
        assert body["error"]["code"] == JSONRPC_INTERNAL_ERROR
        assert "kaboom" in body["error"]["message"]


# ----- configuration -------------------------------------------------


def test_port_override_via_env(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv(PORT_ENV_VAR, "9999")
    server = JsonRpcHttpServer()
    assert server.port == 9999


def test_port_default_when_env_unset(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.delenv(PORT_ENV_VAR, raising=False)
    server = JsonRpcHttpServer()
    assert server.port == DEFAULT_PORT


def test_build_default_server_registers_library_handler(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    server = build_default_server(handler, port=0)
    # The handler should resolve known methods.
    assert server._find_handler("library.list_tracks") is handler
    assert server._find_handler("nope.unknown") is None


# ----- lifecycle -----------------------------------------------------


async def test_serve_can_be_cancelled_cleanly(library: TrackLibrary):
    """``serve()`` should release the socket when its task is cancelled."""
    handler = LibraryRpcHandler(library)
    server = JsonRpcHttpServer(host="127.0.0.1", port=0)
    server.register_handler(handler)

    stop = asyncio.Event()
    task = asyncio.create_task(server.serve(stop_event=stop))
    # Yield once so ``start()`` completes before we ask for stop.
    await asyncio.sleep(0.05)
    stop.set()
    await asyncio.wait_for(task, timeout=2.0)


async def test_start_stop_idempotent(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    server = JsonRpcHttpServer(host="127.0.0.1", port=0)
    server.register_handler(handler)
    await server.start()
    await server.stop()
    # Second stop must not raise.
    await server.stop()


# ----- regression: response shape matches JSON-RPC 2.0 ---------------


async def test_response_envelope_shape(client: TestClient):
    resp = await client.post("/rpc", json=_rpc("library.list_tracks", {}, req_id=42))
    body = await resp.json()
    assert set(body.keys()) == {"jsonrpc", "id", "result"}
    assert body["jsonrpc"] == "2.0"
    assert body["id"] == 42


async def test_error_envelope_shape(client: TestClient):
    resp = await client.post("/rpc", json=_rpc("nope.gone", req_id=7))
    body = await resp.json()
    assert set(body.keys()) == {"jsonrpc", "id", "error"}
    assert body["jsonrpc"] == "2.0"
    assert body["id"] == 7
    assert set(body["error"].keys()) >= {"code", "message"}


# Ensure we don't depend on a stale env var leaking into other tests.
@pytest.fixture(autouse=True)
def _isolate_port_env(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.delenv(PORT_ENV_VAR, raising=False)
    yield
