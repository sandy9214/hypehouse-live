"""JSON-RPC handlers for the ``presets.*`` namespace.

Companion to :mod:`copilot.library_rpc` — same dispatch shape, same
error envelope conventions, just a different table and a different
wire surface. See :mod:`copilot.presets` for the on-disk + JSON
formats this layer projects to / from.

Surface:

* ``presets.save({name, deck_a, deck_b, crossfader_curve})``
    -> ``{preset_id: <int>, preset: <Preset wire shape>}``
* ``presets.list()`` -> ``{presets: [{id, name, created_at}, ...]}``
* ``presets.load({id})`` -> ``{preset: <Preset wire shape>}``
* ``presets.delete({id})`` -> ``{ok: true, deleted: <bool>}``

Errors:

* ``-32602 invalid params`` — bad input shape or duplicate name.
* ``-32603 internal error`` — anything else (re-raised from the store).
"""
from __future__ import annotations

import logging
from typing import Any

from .library import TrackLibrary
from .library_rpc import (
    JSONRPC_INTERNAL_ERROR,
    JSONRPC_INVALID_PARAMS,
    RpcError,
    _coerce_int,
    _coerce_str,
)
from .presets import (
    CROSSFADER_CURVES,
    PresetError,
    PresetStore,
    deck_state_from_wire,
    preset_summary_to_wire,
    preset_to_wire,
)

log = logging.getLogger(__name__)


class PresetRpcHandler:
    """Dispatch ``presets.*`` JSON-RPC methods against a :class:`PresetStore`.

    Transport-agnostic — :class:`copilot.http_server.JsonRpcHttpServer`
    registers this alongside :class:`copilot.library_rpc.LibraryRpcHandler`
    so a single WS / HTTP endpoint serves both namespaces.

    The handler accepts either a :class:`TrackLibrary` (in which case
    it asks for the wired :class:`PresetStore`) or a direct
    :class:`PresetStore` — the latter makes the test path explicit
    (a ``TrackLibrary(":memory:")`` already wires the migration).
    """

    NAMESPACE = "presets"
    METHODS = ("save", "list", "load", "delete")

    def __init__(self, library_or_store: TrackLibrary | PresetStore):
        if isinstance(library_or_store, TrackLibrary):
            self._store = library_or_store.preset_store()
        else:
            self._store = library_or_store

    @property
    def fully_qualified_methods(self) -> tuple[str, ...]:
        return tuple(f"{self.NAMESPACE}.{m}" for m in self.METHODS)

    def handles(self, method: str) -> bool:
        return method in self.fully_qualified_methods

    async def dispatch(
        self, method: str, params: dict[str, Any] | None
    ) -> dict[str, Any]:
        """Run ``method`` with ``params`` and return the result dict.

        ``params`` is normalized to ``{}`` when None, mirroring
        JSON-RPC 2.0's optional-params rule.
        """
        params = params or {}
        if method == "presets.save":
            return self._save(params)
        if method == "presets.list":
            return self._list(params)
        if method == "presets.load":
            return self._load(params)
        if method == "presets.delete":
            return self._delete(params)
        raise RpcError(-32601, f"method not found: {method}")

    # --- handlers -----------------------------------------------------

    def _save(self, params: dict[str, Any]) -> dict[str, Any]:
        name = _coerce_str(params.get("name"), field="name")
        deck_a_raw = params.get("deck_a")
        deck_b_raw = params.get("deck_b")
        if not isinstance(deck_a_raw, dict):
            raise RpcError(
                JSONRPC_INVALID_PARAMS, "deck_a must be an object"
            )
        if not isinstance(deck_b_raw, dict):
            raise RpcError(
                JSONRPC_INVALID_PARAMS, "deck_b must be an object"
            )
        curve = params.get("crossfader_curve")
        if curve is None:
            curve = "Linear"
        if not isinstance(curve, str):
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                "crossfader_curve must be a string",
            )
        if curve not in CROSSFADER_CURVES:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"crossfader_curve must be one of {CROSSFADER_CURVES}",
            )
        deck_a = deck_state_from_wire(deck_a_raw)
        deck_b = deck_state_from_wire(deck_b_raw)
        try:
            saved = self._store.save_preset(
                name=name,
                deck_a=deck_a,
                deck_b=deck_b,
                crossfader_curve=curve,
            )
        except PresetError as exc:
            raise RpcError(
                JSONRPC_INVALID_PARAMS, str(exc), data={"name": name}
            ) from exc
        except Exception as exc:  # noqa: BLE001 — store surface is broad
            log.exception("presets.save failed for %s", name)
            raise RpcError(
                JSONRPC_INTERNAL_ERROR, f"preset save failed: {exc}"
            ) from exc
        return {
            "preset_id": int(saved.id) if saved.id is not None else None,
            "preset": preset_to_wire(saved),
        }

    def _list(self, _params: dict[str, Any]) -> dict[str, Any]:
        try:
            rows = self._store.list_presets()
        except Exception as exc:  # noqa: BLE001
            log.exception("presets.list failed")
            raise RpcError(
                JSONRPC_INTERNAL_ERROR, f"preset list failed: {exc}"
            ) from exc
        return {"presets": [preset_summary_to_wire(p) for p in rows]}

    def _load(self, params: dict[str, Any]) -> dict[str, Any]:
        preset_id = _coerce_int(params.get("id"), field="id", default=-1)
        if preset_id <= 0:
            raise RpcError(
                JSONRPC_INVALID_PARAMS, "id must be a positive integer"
            )
        preset = self._store.load_preset(preset_id)
        if preset is None:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"preset not found: {preset_id}",
                data={"id": preset_id},
            )
        return {"preset": preset_to_wire(preset)}

    def _delete(self, params: dict[str, Any]) -> dict[str, Any]:
        preset_id = _coerce_int(params.get("id"), field="id", default=-1)
        if preset_id <= 0:
            raise RpcError(
                JSONRPC_INVALID_PARAMS, "id must be a positive integer"
            )
        try:
            deleted = self._store.delete_preset(preset_id)
        except Exception as exc:  # noqa: BLE001
            log.exception("presets.delete failed for %d", preset_id)
            raise RpcError(
                JSONRPC_INTERNAL_ERROR, f"preset delete failed: {exc}"
            ) from exc
        # Idempotent — `ok: true` regardless of whether a row existed,
        # so the UI can swap the preset out of its cache without checking.
        return {"ok": True, "deleted": bool(deleted)}
