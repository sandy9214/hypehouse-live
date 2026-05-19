"""JSON-RPC handlers for the ``playlist.*`` namespace.

Wire surface for the auto-DJ queue (see :mod:`copilot.playlist`):

* ``playlist.enqueue({track_id})`` -> ``{entry: PlaylistEntryWire}``
* ``playlist.list()``              -> ``{entries: [PlaylistEntryWire, ...]}``
* ``playlist.reorder({track_id, new_position})`` ->
  ``{entries: [...]}``
* ``playlist.remove({track_id})``  -> ``{entries: [...]}``
* ``playlist.clear()``             -> ``{ok: true}``

Each ``PlaylistEntryWire`` is::

    {
      "track_id":  "kanye-stronger",
      "position":  0,
      "added_at":  "2026-05-18T18:32:01+00:00",
      "track":    <library track wire shape> | null
    }

``track: null`` means the entry references a track id that's no
longer in the library (operator deleted the file post-enqueue). The
UI surfaces these with a "missing" badge + a one-click remove.

The handler shares the same :class:`copilot.playlist.PlaylistQueue`
instance the auto-mix controller consumes from, so an enqueue from
the UI is visible to the next ``AutoMixController.tick`` without any
explicit notification plumbing — both sides read the same SQLite
table.
"""
from __future__ import annotations

import logging
from typing import Any

from .library_rpc import RpcError
from .playlist import PlaylistQueue, entry_to_wire

log = logging.getLogger(__name__)


JSONRPC_INVALID_PARAMS = -32602
JSONRPC_METHOD_NOT_FOUND = -32601


def _coerce_str(value: object, *, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise RpcError(JSONRPC_INVALID_PARAMS, f"{field} must be a non-empty string")
    return value


def _coerce_int(value: object, *, field: str) -> int:
    if value is None:
        raise RpcError(JSONRPC_INVALID_PARAMS, f"{field} is required")
    if isinstance(value, bool):
        # bool is a subclass of int — reject early or "true" becomes 1.
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


class PlaylistRpcHandler:
    """Dispatch ``playlist.*`` JSON-RPC methods against a :class:`PlaylistQueue`.

    Transport-free, mirrors the shape of
    :class:`copilot.library_rpc.LibraryRpcHandler` — callers pass
    ``method`` + ``params`` and get back a ``result`` dict (or an
    :class:`RpcError`).
    """

    NAMESPACE = "playlist"
    METHODS = ("enqueue", "list", "reorder", "remove", "clear")

    def __init__(self, queue: PlaylistQueue):
        self._queue = queue

    @property
    def fully_qualified_methods(self) -> tuple[str, ...]:
        return tuple(f"{self.NAMESPACE}.{m}" for m in self.METHODS)

    def handles(self, method: str) -> bool:
        return method in self.fully_qualified_methods

    async def dispatch(
        self, method: str, params: dict[str, Any] | None
    ) -> dict[str, Any]:
        params = params or {}
        if method == "playlist.enqueue":
            return self._enqueue(params)
        if method == "playlist.list":
            return self._list(params)
        if method == "playlist.reorder":
            return self._reorder(params)
        if method == "playlist.remove":
            return self._remove(params)
        if method == "playlist.clear":
            return self._clear(params)
        raise RpcError(JSONRPC_METHOD_NOT_FOUND, f"method not found: {method}")

    # --- handlers -----------------------------------------------------

    def _enqueue(self, params: dict[str, Any]) -> dict[str, Any]:
        track_id = _coerce_str(params.get("track_id"), field="track_id")
        entry = self._queue.enqueue(track_id)
        return {"entry": entry_to_wire(entry)}

    def _list(self, _params: dict[str, Any]) -> dict[str, Any]:
        entries = self._queue.list_queue()
        return {"entries": [entry_to_wire(e) for e in entries]}

    def _reorder(self, params: dict[str, Any]) -> dict[str, Any]:
        track_id = _coerce_str(params.get("track_id"), field="track_id")
        new_position = _coerce_int(
            params.get("new_position"), field="new_position"
        )
        try:
            entries = self._queue.reorder(track_id, new_position)
        except KeyError as exc:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"track not in queue: {track_id}",
                data={"track_id": track_id},
            ) from exc
        except ValueError as exc:
            raise RpcError(
                JSONRPC_INVALID_PARAMS, str(exc)
            ) from exc
        return {"entries": [entry_to_wire(e) for e in entries]}

    def _remove(self, params: dict[str, Any]) -> dict[str, Any]:
        track_id = _coerce_str(params.get("track_id"), field="track_id")
        try:
            entries = self._queue.remove(track_id)
        except KeyError as exc:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"track not in queue: {track_id}",
                data={"track_id": track_id},
            ) from exc
        return {"entries": [entry_to_wire(e) for e in entries]}

    def _clear(self, _params: dict[str, Any]) -> dict[str, Any]:
        self._queue.clear()
        return {"ok": True}


__all__ = ["PlaylistRpcHandler"]
