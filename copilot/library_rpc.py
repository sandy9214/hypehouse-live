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

import asyncio
import base64
import logging
import sqlite3
from dataclasses import asdict
from pathlib import Path
from typing import TYPE_CHECKING, Any

from .key_match import camelot_to_semitones
from .library import (
    STEMS_STATUS_FAILED,
    STEMS_STATUS_PENDING,
    STEMS_STATUS_READY,
    TrackLibrary,
    TrackRef,
)

if TYPE_CHECKING:  # pragma: no cover — type-only import
    from .streaming import StreamingProvider

log = logging.getLogger(__name__)


# JSON-RPC 2.0 error codes (see docs/api/ws-protocol.md).
JSONRPC_INVALID_PARAMS = -32602
JSONRPC_INTERNAL_ERROR = -32603

# Reserved server-defined error range. We use ``-32000`` for the
# "optional feature not installed" branch — distinct from
# ``-32603 internal error`` because a missing optional dep is a
# configuration problem the user can fix, not an engine bug.
JSONRPC_FEATURE_NOT_INSTALLED = -32000


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
        # 8-slot hot-cue grid — ``int`` ms position per set slot,
        # ``None`` per empty slot. Shape matches the engine's
        # ``Deck::hot_cues: [Option<u64>; 8]`` so a row can be passed
        # straight into the extended ``DeckLoad`` event's
        # ``hot_cues`` field. Built fresh per call so callers can't
        # accidentally mutate the dataclass's default list.
        "hot_cues": [None if c is None else int(c) for c in t.hot_cues],
        # Loudness leveler (schema v7). ``lufs`` = raw integrated
        # loudness; ``track_gain_db`` = engine-ready dB gain to land
        # at -14 LUFS. Both ``null`` for tracks that pre-date the v7
        # ingest; the engine reads null track_gain_db as 0 dB.
        "lufs": None if t.lufs is None else float(t.lufs),
        "track_gain_db": (
            None if t.track_gain_db is None else float(t.track_gain_db)
        ),
        # Schema v9 — row provenance. ``"local"`` for filesystem
        # paths; provider name (``"soundcloud"`` etc.) for streaming
        # rows. UI uses this to render a provider chip on library
        # rows.
        "source": t.source,
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


def _coerce_optional_float(value: object, *, field: str) -> float | None:
    """Parse an optional numeric filter param.

    ``None`` / missing -> ``None`` (no filter applied).
    Numeric (int / float / numeric string) -> ``float``.
    Anything else -> ``RpcError`` with ``-32602``.

    ``bool`` is rejected explicitly because ``isinstance(True, int)``
    is true in Python — without the check, ``"bpm_min": true`` would
    silently become ``1.0`` BPM.
    """
    if value is None:
        return None
    if isinstance(value, bool):
        raise RpcError(JSONRPC_INVALID_PARAMS, f"{field} must be a number")
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError as exc:
            raise RpcError(
                JSONRPC_INVALID_PARAMS, f"{field} must be a number"
            ) from exc
    raise RpcError(JSONRPC_INVALID_PARAMS, f"{field} must be a number")


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
        "set_hot_cues",
        "get_waveform",
        "compute_stems",
        "get_stems",
        "sync_status",
        "sync_now",
        "list_pending_push",
    )
    # ``key_match.compute_offset`` lives in a sibling namespace but
    # needs read access to the same library, so we dispatch it through
    # this handler rather than spinning up a separate RpcHandler class.
    # Wire layer pre-filters via :meth:`handles` which already covers
    # both ``library.*`` and ``key_match.*``.
    #
    # ``streaming.search`` / ``streaming.add_to_library`` are wired
    # through this handler too — same reason (shared library access).
    # The handler keeps a lazy provider registry; missing creds raise
    # ``-32000`` (feature-not-installed) at the dispatch boundary so
    # the UI can pop a "configure SoundCloud" affordance.
    EXTRA_METHODS = (
        "key_match.compute_offset",
        "streaming.search",
        "streaming.add_to_library",
    )

    def __init__(
        self,
        library: TrackLibrary,
        *,
        streaming_providers: (
            "dict[str, StreamingProvider] | None"
        ) = None,
        sync_daemon: object | None = None,
    ):
        self._library = library
        # Optional cloud-sync daemon — when present, `sync_status`
        # folds the last-tick stats into the response so the UI can
        # render "last synced X ago". Typed as `object` to avoid an
        # import cycle (`daemon.py` ships in the same package but the
        # circular reference would force a TYPE_CHECKING shim).
        self._sync_daemon = sync_daemon
        # Per-process registry of in-flight stem-computation tasks,
        # keyed by ``track_id``. Kicking ``library.compute_stems``
        # twice for the same track is a no-op — the second call
        # returns ``{status: "pending"}`` without scheduling a second
        # demucs run. We hold a strong ref so the asyncio loop doesn't
        # garbage-collect the task mid-compute (see
        # https://docs.python.org/3/library/asyncio-task.html#asyncio.create_task).
        self._stem_tasks: dict[str, asyncio.Task[None]] = {}
        # Lazy streaming-provider registry. Tests inject a fake;
        # production construction uses :meth:`_lazy_get_provider`
        # which instantiates the matching client on first access (so
        # an operator without ``$SOUNDCLOUD_CLIENT_ID`` can still
        # boot the co-pilot service — they just can't use streaming).
        self._streaming_providers: dict[str, StreamingProvider] = (
            dict(streaming_providers) if streaming_providers else {}
        )

    @property
    def fully_qualified_methods(self) -> tuple[str, ...]:
        """Public method names as they appear on the wire.

        Includes both the primary ``library.*`` surface and the
        ``key_match.*`` sibling methods dispatched through this
        handler (see :attr:`EXTRA_METHODS`).
        """
        return (
            tuple(f"{self.NAMESPACE}.{m}" for m in self.METHODS)
            + self.EXTRA_METHODS
        )

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
        if method == "library.set_hot_cues":
            return self._set_hot_cues(params)
        if method == "library.get_waveform":
            return self._get_waveform(params)
        if method == "library.compute_stems":
            return self._compute_stems(params)
        if method == "library.get_stems":
            return self._get_stems(params)
        if method == "library.sync_status":
            return self._sync_status(params)
        if method == "library.sync_now":
            return self._sync_now(params)
        if method == "library.list_pending_push":
            return self._list_pending_push(params)
        if method == "key_match.compute_offset":
            return self._key_match_compute_offset(params)
        if method == "streaming.search":
            return self._streaming_search(params)
        if method == "streaming.add_to_library":
            return self._streaming_add_to_library(params)
        raise RpcError(-32601, f"method not found: {method}")

    # --- handlers -----------------------------------------------------

    def _list_pending_push(
        self, _params: dict[str, Any]
    ) -> dict[str, Any]:
        """Return the set of track IDs awaiting a cloud push.

        Used by the UI library table to render a per-row "pending
        sync" indicator. Cheap — `pending_push_ids()` is a single
        SQLite SELECT against a primary-key index. Returned as a
        list (JSON has no native set type) but the UI builds a Set
        client-side for O(1) membership checks.
        """
        return {"ids": list(self._library.pending_push_ids())}

    def _sync_now(self, _params: dict[str, Any]) -> dict[str, Any]:
        """Fire a single out-of-band sync tick and return fresh status.

        Used by the AboutPanel "Sync now" button so an operator can
        force a pull/push without waiting for the daemon's next
        scheduled tick. The daemon's `tick_once` is the same method
        the loop calls — same locking, same stats-update path. We
        catch the same exception classes the daemon loop swallows so
        that a transient cloud failure surfaces as a useful RPC error
        instead of crashing the WS handler.

        Returns the post-tick `sync_status` payload (same shape as
        `library.sync_status`) so the UI can update without a second
        round trip.

        Raises ``-32000`` when the daemon isn't wired (local-only
        mode); the UI hides the button in that case but the explicit
        error is the right contract for any other caller.
        """
        if self._sync_daemon is None:
            raise RpcError(
                JSONRPC_FEATURE_NOT_INSTALLED,
                "cloud sync not configured",
            )
        # Lazy-imports to avoid pulling cloud_sync into the cold-start
        # path of test fixtures that don't exercise sync.
        from .cloud_sync.client import SyncError as _SyncError

        try:
            self._sync_daemon.tick_once()  # type: ignore[attr-defined]
        except _SyncError as exc:
            raise RpcError(
                JSONRPC_INTERNAL_ERROR,
                f"cloud sync transport error: {exc}",
            ) from exc
        except sqlite3.Error as exc:
            raise RpcError(
                JSONRPC_INTERNAL_ERROR,
                f"cloud sync local DB error: {exc}",
            ) from exc
        return self._sync_status(_params)

    def _sync_status(self, _params: dict[str, Any]) -> dict[str, Any]:
        """Cloud library sync status snapshot (#102 follow-up).

        Always returns ``pending_push_count`` + ``library_track_count``.
        When a `SyncDaemon` is wired (production path), folds in the
        last-tick stats (`last_pull_micros`, `last_push_micros`,
        `last_pull_fetched`, `last_pull_applied`, `last_push_pushed`,
        `last_tick_error`). Returns `0` / `""` defaults when the
        daemon isn't wired (test path / pre-cloud-sync local-only
        runs).
        """
        out: dict[str, Any] = {
            "pending_push_count": len(self._library.pending_push_ids()),
            "library_track_count": self._library.count_tracks(),
        }
        if self._sync_daemon is not None:
            stats = self._sync_daemon.stats()  # type: ignore[attr-defined]
            out.update(
                {
                    "last_pull_micros": int(stats.last_pull_micros),
                    "last_push_micros": int(stats.last_push_micros),
                    "last_pull_fetched": int(stats.last_pull_fetched),
                    "last_pull_applied": int(stats.last_pull_applied),
                    "last_push_pushed": int(stats.last_push_pushed),
                    "last_tick_error": str(stats.last_tick_error),
                    "next_sync_micros": int(stats.next_sync_micros),
                }
            )
        else:
            out.update(
                {
                    "last_pull_micros": 0,
                    "last_push_micros": 0,
                    "last_pull_fetched": 0,
                    "last_pull_applied": 0,
                    "last_push_pushed": 0,
                    "last_tick_error": "",
                    "next_sync_micros": 0,
                }
            )
        return out

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
        # Smart-filter params (optional, layered onto the search). See
        # ``TrackLibrary.search_tracks`` for the composition rules — UI
        # uses these for chip-based filters (BPM range slider +
        # "compatible with" track picker).
        bpm_min = _coerce_optional_float(
            params.get("bpm_min"), field="bpm_min"
        )
        bpm_max = _coerce_optional_float(
            params.get("bpm_max"), field="bpm_max"
        )
        compat_raw = params.get("compatible_with_track_id")
        compatible_with_track_id: str | None
        if compat_raw is None:
            compatible_with_track_id = None
        else:
            # Non-empty string required when present (a None / missing
            # field skips the filter entirely; passing "" explicitly
            # is a usage error).
            compatible_with_track_id = _coerce_str(
                compat_raw, field="compatible_with_track_id"
            )
        tracks = self._library.search_tracks(
            query,
            limit=limit,
            bpm_min=bpm_min,
            bpm_max=bpm_max,
            compatible_with_track_id=compatible_with_track_id,
        )
        return {
            "tracks": [track_ref_to_wire(t) for t in tracks],
            "query": query,
            "limit": max(1, min(limit, 1000)),
        }

    def _set_hot_cues(self, params: dict[str, Any]) -> dict[str, Any]:
        """Persist a new 8-slot hot-cue array for ``track_id``.

        Wire shape::

            { "track_id": "...", "hot_cues": [int|null, ... * 8] }

        Returns ``{ "track": <TrackRef wire shape> }`` mirroring
        ``library.add_track`` so the UI can swap the row into its
        cache without a follow-up fetch.
        """
        track_id = _coerce_str(params.get("track_id"), field="track_id")
        raw_cues = params.get("hot_cues")
        if not isinstance(raw_cues, list):
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                "hot_cues must be a list",
            )
        try:
            ref = self._library.set_hot_cues(track_id, raw_cues)
        except ValueError as exc:
            # Shape errors (length / type / negative) surface as
            # -32602; the wire layer translates these into the proper
            # JSON-RPC error envelope.
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                str(exc),
                data={"track_id": track_id},
            ) from exc
        except KeyError as exc:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"track not found: {track_id}",
                data={"track_id": track_id},
            ) from exc
        return {"track": track_ref_to_wire(ref)}

    def _get_waveform(self, params: dict[str, Any]) -> dict[str, Any]:
        """Return base64-encoded peak-pairs bytes for ``track_id``.

        Wire shape::

            { "track_id": "..." }

        Returns::

            { "track_id": "...", "peaks_b64": "<base64>" }   # success
            { "track_id": "...", "peaks_b64": null }         # missing/un-analyzed

        Lazy-compute path: if the row exists but ``waveform_peaks`` is
        NULL (e.g. a pre-v4 row that wasn't re-analyzed), attempt to
        compute peaks from the on-disk audio path *now* and persist
        them. A compute failure (file moved, codec missing) returns
        ``peaks_b64: null`` rather than an error envelope so the UI's
        flat-line fallback path still works.

        Why base64 rather than a binary frame: keeps the wire shape a
        plain JSON-RPC ``result`` dict, no out-of-band framing. 2000
        peak pairs = 4000 bytes → ~5400 b64 chars, well under any
        practical JSON message size limit.
        """
        track_id = _coerce_str(params.get("track_id"), field="track_id")
        ref = self._library.get(track_id)
        if ref is None:
            # No such track. Return a "not found" envelope rather than
            # an error — keeps the UI's single fetch path simple
            # (always check ``peaks_b64 != null`` before rendering).
            return {"track_id": track_id, "peaks_b64": None}

        peaks = self._library.get_waveform(track_id)
        if peaks is None:
            # Lazy compute on first request. Wrapped in a broad except
            # because the underlying audio file might be unreachable
            # (NFS mount lost, file deleted post-ingest) — the UI
            # falls back gracefully on null.
            try:
                from .waveform import compute_peaks  # lazy librosa import

                computed = compute_peaks(Path(ref.path))
            except Exception:  # noqa: BLE001 — peaks are best-effort
                log.warning(
                    "get_waveform: lazy compute failed for %s", track_id
                )
                return {"track_id": track_id, "peaks_b64": None}
            # Persist so the next request is a fast read.
            try:
                self._library.set_waveform(track_id, computed)
            except KeyError:
                # Track removed between the .get and the set — rare race.
                pass
            peaks = computed

        return {
            "track_id": track_id,
            "peaks_b64": base64.b64encode(peaks).decode("ascii"),
        }

    # --- stem separation (v5 schema) ---------------------------------

    async def _run_stem_task(self, track_id: str) -> None:
        """Background coroutine that runs the heavy demucs call.

        SQLite connections are pinned to the thread that opened them
        (``check_same_thread=True`` default), so we can't just shove
        :meth:`TrackLibrary.compute_track_stems` into
        :func:`asyncio.to_thread`. Instead we split the work:

        * Status writes (``pending`` → ``ready`` / ``failed``) and the
          track lookup happen on the event-loop thread (synchronous
          SQLite calls — they're sub-millisecond).
        * The heavy demucs invocation runs in
          :func:`asyncio.to_thread` so it doesn't block the loop.

        All exceptions are caught — the ``"failed"`` status is
        persisted explicitly so subsequent ``library.get_stems``
        calls report the failure correctly.
        """
        from . import stems as stems_mod

        try:
            ref = self._library.get(track_id)
            if ref is None:
                # Row vanished between scheduling + run — rare. Nothing
                # to persist; just bail.
                return

            root = stems_mod.default_stems_root()
            track_dir = root / track_id
            self._library.set_stems(
                track_id,
                status=STEMS_STATUS_PENDING,
                stems_dir=str(track_dir),
            )

            try:
                await asyncio.to_thread(
                    stems_mod.compute_stems, Path(ref.path), track_dir
                )
            except Exception:  # noqa: BLE001 — see below
                self._library.set_stems(
                    track_id,
                    status=STEMS_STATUS_FAILED,
                    stems_dir=str(track_dir),
                )
                raise
            self._library.set_stems(
                track_id,
                status=STEMS_STATUS_READY,
                stems_dir=str(track_dir),
            )
        except Exception:  # noqa: BLE001 — outer guard
            log.exception("stem computation failed for %s", track_id)
        finally:
            # Drop the task from the registry so a follow-up call
            # (e.g. after a "failed" status) can re-schedule.
            self._stem_tasks.pop(track_id, None)

    def _compute_stems(self, params: dict[str, Any]) -> dict[str, Any]:
        """Kick off stem separation for ``track_id`` as a background task.

        Returns immediately with ``{status: "pending", track_id: ...}``
        — the caller polls ``library.get_stems`` to learn when the
        computation finishes. The actual demucs invocation runs in a
        worker thread (see :meth:`_run_stem_task`).

        Errors:

        * ``-32602`` — ``track_id`` missing / unknown.
        * ``-32000`` — demucs not installed (raised by
          :class:`copilot.stems.StemsDependencyError`). The probe
          import is done synchronously here so the user gets the
          install hint on the *first* call rather than via a status
          flip to ``"failed"`` after a polling round-trip.
        """
        track_id = _coerce_str(params.get("track_id"), field="track_id")
        if self._library.get(track_id) is None:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"track not found: {track_id}",
                data={"track_id": track_id},
            )

        # Probe the optional dep synchronously so we can surface the
        # install hint immediately. Lazy-imported so module load
        # doesn't pay the torch import cost.
        try:
            import demucs.api  # type: ignore[import-not-found]  # noqa: F401
        except ImportError as exc:
            raise RpcError(
                JSONRPC_FEATURE_NOT_INSTALLED,
                "stems feature not installed: "
                "pip install hypehouse-copilot[stems]",
                data={"track_id": track_id},
            ) from exc

        # De-dupe in-flight requests. A second call while a task is
        # running returns the same {pending} envelope — UI doesn't
        # have to track its own "did I already ask?" state.
        if track_id in self._stem_tasks and not self._stem_tasks[track_id].done():
            return {"track_id": track_id, "status": STEMS_STATUS_PENDING}

        task = asyncio.create_task(self._run_stem_task(track_id))
        self._stem_tasks[track_id] = task
        return {"track_id": track_id, "status": STEMS_STATUS_PENDING}

    def _get_stems(self, params: dict[str, Any]) -> dict[str, Any]:
        """Return the current stem-cache state for ``track_id``.

        Wire shape::

            { "track_id": "...",
              "status":   "ready" | "pending" | "failed" | null,
              "stems":    { "vocals": "...", "drums": "...",
                            "bass":   "...", "other": "..." } | null }

        ``status: null`` + ``stems: null`` means the track exists but
        stems have never been requested. ``status: "ready"`` with
        ``stems`` populated is the success case the engine integration
        will consume.

        A missing track returns ``{status: null, stems: null}`` rather
        than an error — mirrors :meth:`_get_waveform`'s graceful
        degradation so the UI's single fetch path stays simple.
        """
        track_id = _coerce_str(params.get("track_id"), field="track_id")
        info = self._library.get_stems_status(track_id)
        if info is None:
            # Track row doesn't exist — graceful null.
            return {"track_id": track_id, "status": None, "stems": None}

        status, stems_dir = info
        if status != STEMS_STATUS_READY or stems_dir is None:
            return {
                "track_id": track_id,
                "status": status,
                "stems": None,
            }

        # Re-resolve the four stem paths from the on-disk cache so the
        # response reflects current filesystem reality (e.g. an
        # operator could have nuked the cache between compute + get).
        from .stems import STEM_NAMES

        cache_dir = Path(stems_dir)
        stems_map: dict[str, str] = {}
        for name in STEM_NAMES:
            wav = cache_dir / f"{name}.wav"
            if not wav.exists():
                # Cache directory exists but a stem is missing — flip
                # the status to "failed" so the UI can offer a retry
                # button. Don't raise; the caller wants a status, not
                # an error envelope.
                try:
                    self._library.set_stems(
                        track_id,
                        status=STEMS_STATUS_FAILED,
                        stems_dir=stems_dir,
                    )
                except KeyError:
                    pass
                return {
                    "track_id": track_id,
                    "status": STEMS_STATUS_FAILED,
                    "stems": None,
                }
            stems_map[name] = str(wav)
        return {
            "track_id": track_id,
            "status": STEMS_STATUS_READY,
            "stems": stems_map,
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

    # --- key_match.* -------------------------------------------------

    def _key_match_compute_offset(
        self, params: dict[str, Any]
    ) -> dict[str, Any]:
        """Compute the semitone offset to pitch ``from_track`` into ``to_track``'s key.

        Wire shape::

            { "from_track_id": "...", "to_track_id": "..." }

        Returns::

            { "semitones": <float in [-6.0, 6.0]> }

        Missing / unknown track ids surface as ``-32602`` rather than a
        silent 0 — the UI button is supposed to be disabled until both
        decks have a loaded library row, so a fetch here means the
        client got out of sync and a loud error is the right behavior.

        Tracks with an unparseable ``camelot_key`` (e.g. ``"?"`` from
        a failed analyzer pass) return ``{semitones: 0.0}`` — the
        underlying :func:`camelot_to_semitones` is lenient on malformed
        codes so the UI can gracefully degrade to "no shift" rather
        than fail the whole click.
        """
        from_id = _coerce_str(params.get("from_track_id"), field="from_track_id")
        to_id = _coerce_str(params.get("to_track_id"), field="to_track_id")
        from_ref = self._library.get(from_id)
        if from_ref is None:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"track not found: {from_id}",
                data={"track_id": from_id},
            )
        to_ref = self._library.get(to_id)
        if to_ref is None:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"track not found: {to_id}",
                data={"track_id": to_id},
            )
        offset = camelot_to_semitones(from_ref.camelot_key, to_ref.camelot_key)
        return {"semitones": float(offset)}

    # --- streaming.* -------------------------------------------------

    def _lazy_get_provider(self, name: str) -> "StreamingProvider":
        """Return the cached provider client for ``name`` (lazy-instantiate).

        Constructed on first request rather than at handler-init time
        so an operator without streaming creds can still boot the
        co-pilot — the only failure is at the moment they try to use
        the feature, which is when a UI affordance can guide them
        through the apply-for-key flow.

        Raises:
            RpcError: ``-32602`` unknown provider; ``-32000`` provider
                creds not configured (the
                :class:`StreamingAuthError` from the client's __init__
                is translated here).
        """
        # Lazy import — the streaming module pulls urllib only, but
        # keeping it lazy means the existing library_rpc tests don't
        # import network code at collection time.
        from .streaming import StreamingAuthError
        from .streaming.soundcloud import SoundCloudClient

        cached = self._streaming_providers.get(name)
        if cached is not None:
            return cached

        client: "StreamingProvider"
        if name == "soundcloud":
            try:
                client = SoundCloudClient()
            except StreamingAuthError as exc:
                # Surface the apply-for-key hint to the UI via the
                # JSON-RPC error envelope's ``message`` field. The
                # ``-32000`` code mirrors the missing-demucs branch in
                # ``compute_stems`` so the UI's single "configure
                # optional feature" handler covers both.
                raise RpcError(
                    JSONRPC_FEATURE_NOT_INSTALLED,
                    str(exc),
                    data={"provider": name},
                ) from exc
        else:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"unknown streaming provider: {name}",
                data={"provider": name},
            )
        self._streaming_providers[name] = client
        return client

    def _streaming_search(self, params: dict[str, Any]) -> dict[str, Any]:
        """Search a streaming provider's catalog.

        Wire shape::

            { "provider": "soundcloud", "query": "lo-fi", "limit": 20 }

        Returns::

            { "provider": "soundcloud",
              "query":    "lo-fi",
              "results":  [ { "id": "...", "title": "...", ... }, ... ] }

        Errors:
            * ``-32602`` — missing / malformed param, unknown provider.
            * ``-32000`` — provider creds not configured (apply-for-key
              hint in ``message``).
            * ``-32603`` — provider API error (network / 5xx).
        """
        provider_name = _coerce_str(
            params.get("provider"), field="provider"
        )
        query = _coerce_str(
            params.get("query"), field="query", allow_empty=True
        )
        limit = _coerce_int(params.get("limit"), field="limit", default=20)
        provider = self._lazy_get_provider(provider_name)
        # Lazy import for the typed exception — same reason as the
        # provider lookup; keeps library_rpc importable when streaming
        # is unused.
        from .streaming import StreamingAuthError, StreamingError

        try:
            results = provider.search(query, limit=limit)
        except StreamingAuthError as exc:
            raise RpcError(
                JSONRPC_FEATURE_NOT_INSTALLED,
                str(exc),
                data={"provider": provider_name},
            ) from exc
        except StreamingError as exc:
            raise RpcError(
                JSONRPC_INTERNAL_ERROR,
                f"streaming provider error: {exc}",
                data={"provider": provider_name},
            ) from exc
        return {
            "provider": provider_name,
            "query": query,
            "results": [asdict(t) for t in results],
        }

    def _streaming_add_to_library(
        self, params: dict[str, Any]
    ) -> dict[str, Any]:
        """Persist a streaming track into the library.

        Wire shape::

            { "provider":  "soundcloud",
              "track_id":  "<provider-scoped id>",
              "metadata":  { "title": "...", "artist": "...",
                             "duration_s": 234.0,
                             "genre": "...", "license": "cc-by",
                             "key": "8B"  } }

        ``metadata`` is the same shape :meth:`_streaming_search`
        returns per result (an ``asdict`` of :class:`StreamingTrack`).
        Caller round-trips the metadata so the library doesn't need a
        second provider call at add time.

        Returns::

            { "track": <TrackRef wire shape> }

        Errors:
            * ``-32602`` — missing fields / malformed types / non-CC
              ``license`` (defence-in-depth — search should have
              filtered, but we re-check at the trust boundary).
        """
        from .streaming import is_cc_license

        provider_name = _coerce_str(
            params.get("provider"), field="provider"
        )
        track_id = _coerce_str(params.get("track_id"), field="track_id")
        metadata = params.get("metadata")
        if not isinstance(metadata, dict):
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                "metadata must be an object",
            )
        # Re-check license — the search-time filter is best effort;
        # this is the library trust boundary. ARR / unknown licenses
        # are rejected with a loud error so a buggy provider can't
        # poison the catalog.
        license_str = metadata.get("license")
        if not isinstance(license_str, str) or not is_cc_license(license_str):
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                f"license must be Creative Commons, got {license_str!r}",
                data={"license": license_str},
            )
        title_raw = metadata.get("title")
        if not isinstance(title_raw, str) or not title_raw:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                "metadata.title must be a non-empty string",
            )
        artist_raw = metadata.get("artist")
        artist = artist_raw if isinstance(artist_raw, str) else ""
        duration_raw = metadata.get("duration_s")
        if not isinstance(duration_raw, (int, float)) or isinstance(
            duration_raw, bool
        ):
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                "metadata.duration_s must be a number",
            )
        stream_url_raw = metadata.get("stream_url")
        if not isinstance(stream_url_raw, str) or not stream_url_raw:
            raise RpcError(
                JSONRPC_INVALID_PARAMS,
                "metadata.stream_url must be a non-empty string",
            )
        key_raw = metadata.get("key")
        camelot_key = key_raw if isinstance(key_raw, str) else None
        genre_raw = metadata.get("genre")
        genre = genre_raw if isinstance(genre_raw, str) else ""

        ref = self._library.add_streaming_track(
            provider=provider_name,
            track_id=track_id,
            title=title_raw,
            artist=artist,
            duration_s=float(duration_raw),
            stream_url=stream_url_raw,
            camelot_key=camelot_key,
            genre=genre,
        )
        return {"track": track_ref_to_wire(ref)}
