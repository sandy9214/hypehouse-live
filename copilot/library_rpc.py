"""JSON-RPC handlers for the ``library.*`` namespace.

This module owns the wire surface the UI calls when it wants to browse
the track catalog, search it, or kick off an analyzer scan. The
handlers are kept transport-agnostic — each takes a plain ``params``
dict and returns a plain ``result`` dict. Callers wire them into
whatever WS / HTTP server they like.

Surface:

* ``library.list_tracks`` — paginated catalog dump
  ``{tracks: [TrackRef, ...], total: <int>, limit: <int>, offset: <int>}``.
* ``library.add_track`` — analyze a single local path; returns the
  new TrackRef.
* ``library.search_tracks`` — substring + ``key:`` + ``bpm:lo-hi``
  shorthand search.
* ``library.add_track_from_directory`` — scan a server-side directory
  and analyze every file (idempotent). v0.1's primary ingest path
  because the browser file picker can't surface server-resolvable
  paths.

Wire shape of one TrackRef:

```json
{
  "id":                  "kanye-stronger",
  "path":                "/music/kanye-stronger.mp3",
  "bpm":                 124.0,
  "camelot_key":         "8B",
  "energy":              0.21,
  "duration_s":          265.3,
  "beat_grid_anchor_ms": 0,
  "beat_period_ms":      483.87,
  "downbeats_ms":        [0, 1935, 3870, ...]
}
```

The shape is identical to :class:`copilot.library.TrackRef` minus the
``track_id``->``id`` rename which keeps the wire field consistent with
the engine's ``state::TrackRef`` (``id`` + ``path``). UI code maps
straight into a DeckLoad event using ``id`` + ``path`` plus the BPM /
anchor / downbeats fields the engine needs.
"""
from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from .library import TrackLibrary, TrackRef

log = logging.getLogger(__name__)


# JSON-RPC 2.0 error codes (see docs/api/ws-protocol.md).
JSONRPC_INVALID_PARAMS = -32602
JSONRPC_INTERNAL_ERROR = -32603


class RpcError(Exception):
    """Raised by handlers to signal a JSON-RPC-shaped error.

    Caller (transport layer) converts this into a proper response
    envelope. Keeping it as an exception keeps the happy path one
    function call deep instead of result-tuples everywhere.
    """

    def __init__(self, code: int, message: str, data: object | None = None):
        super().__init__(message)
        self.code = int(code)
        self.message = str(message)
        self.data = data


def track_ref_to_wire(t: TrackRef) -> dict[str, Any]:
    """Project a :class:`TrackRef` into the wire dict the UI consumes.

    Field rename ``track_id`` -> ``id`` aligns the library wire shape
    with the engine's ``state::TrackRef`` (also ``id`` + ``path``) so a
    UI library row can be passed verbatim into a ``DeckLoad`` event's
    ``track`` field with no per-field plumbing.
    """
    return {
        "id": t.track_id,
        "path": t.path,
        "bpm": float(t.bpm),
        "camelot_key": t.camelot_key,
        "energy": float(t.energy),
        "duration_s": float(t.duration_s),
        "beat_grid_anchor_ms": int(t.beat_grid_anchor_ms),
        "beat_period_ms": float(t.beat_period_ms),
        "downbeats_ms": [int(d) for d in t.downbeats_ms],
    }


def _coerce_int(value: object, *, field: str, default: int) -> int:
    if value is None:
        return default
    if isinstance(value, bool):  # bool is a subclass of int — reject explicitly
        raise RpcError(JSONRPC_INVALID_PARAMS, f"{field} must be an int")
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value)
        except ValueError as exc:
            raise RpcError(
                JSONRPC_INVALID_PARAMS, f"{field} must be an int"
            ) from exc
    raise RpcError(JSONRPC_INVALID_PARAMS, f"{field} must be an int")


def _coerce_str(value: object, *, field: str, allow_empty: bool = False) -> str:
    if value is None and allow_empty:
        return ""
    if not isinstance(value, str):
        raise RpcError(JSONRPC_INVALID_PARAMS, f"{field} must be a string")
    if not allow_empty and not value:
        raise RpcError(JSONRPC_INVALID_PARAMS, f"{field} must be non-empty")
    return value


class LibraryRpcHandler:
    """Dispatch ``library.*`` JSON-RPC methods against a :class:`TrackLibrary`.

    Transport-free: callers pass ``method`` + ``params`` and get back a
    ``result`` dict (or an :class:`RpcError`). This makes unit tests
    fast and lets the same handler wire into aiohttp, websockets, or
    a future Tauri command dispatcher without changing the core logic.
    """

    NAMESPACE = "library"
    METHODS = (
        "list_tracks",
        "add_track",
        "search_tracks",
        "add_track_from_directory",
    )

    def __init__(self, library: TrackLibrary):
        self._library = library

    @property
    def fully_qualified_methods(self) -> tuple[str, ...]:
        """Public method names as they appear on the wire (``library.<m>``)."""
        return tuple(f"{self.NAMESPACE}.{m}" for m in self.METHODS)

    def handles(self, method: str) -> bool:
        return method in self.fully_qualified_methods

    async def dispatch(
        self, method: str, params: dict[str, Any] | None
    ) -> dict[str, Any]:
        """Run ``method`` with ``params`` and return the result dict.

        ``params`` is normalized to ``{}`` when None, mirroring
        JSON-RPC 2.0's optional-params rule. Unknown methods raise
        :class:`RpcError` with ``-32601 method not found`` — but the
        transport layer should normally pre-filter via :meth:`handles`.
        """
        params = params or {}
        if method == "library.list_tracks":
            return self._list_tracks(params)
        if method == "library.add_track":
            return self._add_track(params)
        if method == "library.search_tracks":
            return self._search_tracks(params)
        if method == "library.add_track_from_directory":
            return self._add_track_from_directory(params)
        raise RpcError(-32601, f"method not found: {method}")

    # --- handlers -----------------------------------------------------

    def _list_tracks(self, params: dict[str, Any]) -> dict[str, Any]:
        limit = _coerce_int(params.get("limit"), field="limit", default=100)
        offset = _coerce_int(params.get("offset"), field="offset", default=0)
        tracks = self._library.list_tracks(limit=limit, offset=offset)
        return {
            "tracks": [track_ref_to_wire(t) for t in tracks],
            "total": self._library.count_tracks(),
            "limit": max(1, min(limit, 1000)),
            "offset": max(0, offset),
        }

    def _add_track(self, params: dict[str, Any]) -> dict[str, Any]:
        path = _coerce_str(params.get("path"), field="path")
        path_obj = Path(path).expanduser()
        if not path_obj.exists():
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"path does not exist: {path_obj}",
                data={"path": str(path_obj)},
            )
        if not path_obj.is_file():
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"path is not a file: {path_obj}",
                data={"path": str(path_obj)},
            )
        try:
            ref = self._library.add_track_from_path(path_obj)
        except Exception as exc:  # noqa: BLE001 — analyzer surface is broad
            log.exception("add_track failed for %s", path_obj)
            raise RpcError(
                JSONRPC_INTERNAL_ERROR,
                f"analyzer failed: {exc}",
                data={"path": str(path_obj)},
            ) from exc
        return {"track": track_ref_to_wire(ref)}

    def _search_tracks(self, params: dict[str, Any]) -> dict[str, Any]:
        query = _coerce_str(
            params.get("query"), field="query", allow_empty=True
        )
        limit = _coerce_int(params.get("limit"), field="limit", default=100)
        tracks = self._library.search_tracks(query, limit=limit)
        return {
            "tracks": [track_ref_to_wire(t) for t in tracks],
            "query": query,
            "limit": max(1, min(limit, 1000)),
        }

    def _add_track_from_directory(
        self, params: dict[str, Any]
    ) -> dict[str, Any]:
        path = _coerce_str(params.get("path"), field="path")
        path_obj = Path(path).expanduser()
        if not path_obj.exists() or not path_obj.is_dir():
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"not a directory: {path_obj}",
                data={"path": str(path_obj)},
            )
        try:
            added = self._library.add_tracks_from_directory(path_obj)
        except Exception as exc:  # noqa: BLE001 — analyzer surface is broad
            log.exception("add_track_from_directory failed for %s", path_obj)
            raise RpcError(
                JSONRPC_INTERNAL_ERROR,
                f"scan failed: {exc}",
                data={"path": str(path_obj)},
            ) from exc
        return {
            "added": [track_ref_to_wire(t) for t in added],
            "added_count": len(added),
            "total": self._library.count_tracks(),
        }
