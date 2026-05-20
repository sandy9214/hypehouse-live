"""Library JSON-RPC handler tests.

Covers the four methods exposed by :class:`copilot.library_rpc.LibraryRpcHandler`:
``library.list_tracks``, ``library.add_track``, ``library.search_tracks``,
``library.add_track_from_directory``.

Heavy analyzer paths (``add_track_from_path`` / ``add_tracks_from_directory``)
are exercised via monkeypatching the analyzer wrapper so the tests run
without librosa/madmom CPU.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from copilot.library import TrackLibrary, TrackRef
from copilot.library_rpc import (
    JSONRPC_INVALID_PARAMS,
    LibraryRpcHandler,
    RpcError,
    track_ref_to_wire,
)

# Most tests in this module are async (they exercise the
# ``LibraryRpcHandler.dispatch`` coroutine). The two
# ``test_track_ref_to_wire_*`` / ``test_handler_handles_*`` tests at the
# bottom are sync and explicitly opt out of the marker.
_asyncio = pytest.mark.asyncio


def _seed(lib: TrackLibrary) -> list[TrackRef]:
    rows = [
        TrackRef("alpha", "/m/alpha.mp3", 120.0, "8B", 0.2, 200.0),
        TrackRef("bravo", "/m/bravo.mp3", 124.0, "8B", 0.3, 210.0),
        TrackRef("charlie", "/m/charlie.mp3", 128.0, "9B", 0.4, 220.0),
        TrackRef("delta", "/m/delta.mp3", 130.0, "10A", 0.5, 230.0),
        TrackRef("echo", "/m/echo.mp3", 140.0, "11A", 0.6, 240.0),
    ]
    for r in rows:
        lib.add_track(r)
    return rows


# ---- list_tracks ----------------------------------------------------


@_asyncio
async def test_list_tracks_returns_all_with_total(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.list_tracks", {})
    assert result["total"] == 5
    assert len(result["tracks"]) == 5
    assert result["limit"] == 100
    assert result["offset"] == 0
    # Tracks ordered by id alphabetically (stable for scroll).
    ids = [t["id"] for t in result["tracks"]]
    assert ids == sorted(ids)


@_asyncio
async def test_list_tracks_paginates(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    page1 = await handler.dispatch(
        "library.list_tracks", {"limit": 2, "offset": 0}
    )
    page2 = await handler.dispatch(
        "library.list_tracks", {"limit": 2, "offset": 2}
    )
    page3 = await handler.dispatch(
        "library.list_tracks", {"limit": 2, "offset": 4}
    )
    page_empty = await handler.dispatch(
        "library.list_tracks", {"limit": 2, "offset": 10}
    )
    assert [t["id"] for t in page1["tracks"]] == ["alpha", "bravo"]
    assert [t["id"] for t in page2["tracks"]] == ["charlie", "delta"]
    assert [t["id"] for t in page3["tracks"]] == ["echo"]
    assert page_empty["tracks"] == []
    # Total is page-independent — UI uses it for the scrollbar.
    assert page1["total"] == page2["total"] == 5


@_asyncio
async def test_list_tracks_validates_limit_type(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch("library.list_tracks", {"limit": "not-a-number"})
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_list_tracks_clamps_extreme_limit(library: TrackLibrary):
    """A negative or huge limit shouldn't crash — clamp silently."""
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.list_tracks", {"limit": -5, "offset": 0}
    )
    # Clamped up to 1 — we still get one row back.
    assert result["limit"] == 1
    assert len(result["tracks"]) == 1


# ---- search_tracks --------------------------------------------------


@_asyncio
async def test_search_substring_matches_id_or_path(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.search_tracks", {"query": "echo"})
    assert [t["id"] for t in result["tracks"]] == ["echo"]


@_asyncio
async def test_search_key_shorthand(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.search_tracks", {"query": "key:8B"})
    assert sorted(t["id"] for t in result["tracks"]) == ["alpha", "bravo"]


@_asyncio
async def test_search_bpm_range_shorthand(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.search_tracks", {"query": "bpm:124-130"}
    )
    assert sorted(t["id"] for t in result["tracks"]) == ["bravo", "charlie", "delta"]


@_asyncio
async def test_search_empty_query_returns_all(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.search_tracks", {"query": ""})
    assert len(result["tracks"]) == 5


# ---- smart filters: bpm_min / bpm_max / compatible_with_track_id ---


@_asyncio
async def test_search_bpm_min_max_inclusive_range(library: TrackLibrary):
    """Structured ``bpm_min`` + ``bpm_max`` filter (chip UI surface).

    Seed BPMs are 120/124/128/130/140 — a 124..130 inclusive range
    must return exactly bravo/charlie/delta.
    """
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.search_tracks",
        {"query": "", "bpm_min": 124, "bpm_max": 130},
    )
    assert sorted(t["id"] for t in result["tracks"]) == [
        "bravo",
        "charlie",
        "delta",
    ]


@_asyncio
async def test_search_bpm_only_one_bound(library: TrackLibrary):
    """A missing bound means "open on that side"."""
    _seed(library)
    handler = LibraryRpcHandler(library)
    high = await handler.dispatch(
        "library.search_tracks", {"query": "", "bpm_min": 130}
    )
    assert sorted(t["id"] for t in high["tracks"]) == ["delta", "echo"]
    low = await handler.dispatch(
        "library.search_tracks", {"query": "", "bpm_max": 124}
    )
    assert sorted(t["id"] for t in low["tracks"]) == ["alpha", "bravo"]


@_asyncio
async def test_search_compatible_with_returns_within_2_camelot(
    library: TrackLibrary,
):
    """Camelot-distance gate (≤ 2) excludes distant keys.

    Reference = ``alpha`` (8B). Distances:
      * alpha 8B  -> 0  (excluded — reference itself)
      * bravo 8B  -> 0  (compatible, but reference itself dropped)
      * charlie 9B -> 1 (compatible)
      * delta 10A  -> 3 (NOT compatible — gated out)
      * echo 11A   -> 4 (NOT compatible — gated out)
    """
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.search_tracks",
        {"query": "", "compatible_with_track_id": "alpha"},
    )
    ids = sorted(t["id"] for t in result["tracks"])
    assert "alpha" not in ids  # reference itself is filtered
    assert ids == ["bravo", "charlie"]


@_asyncio
async def test_search_compatible_with_combined_with_bpm_range(
    library: TrackLibrary,
):
    """Filter composition — chip + chip must AND together."""
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.search_tracks",
        {
            "query": "",
            "compatible_with_track_id": "alpha",
            "bpm_min": 125,
        },
    )
    # alpha excluded (self); bravo BPM=124 < 125 -> dropped; charlie
    # passes both gates (9B + 128 BPM).
    assert [t["id"] for t in result["tracks"]] == ["charlie"]


@_asyncio
async def test_search_compatible_with_unknown_track_id_returns_empty(
    library: TrackLibrary,
):
    """An unknown reference id degrades to no matches (not an error).

    Matches the spec: filter is best-effort, UI surfaces empty list +
    "no matches" hint rather than an error banner.
    """
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.search_tracks",
        {"query": "", "compatible_with_track_id": "nope"},
    )
    assert result["tracks"] == []


@_asyncio
async def test_search_bpm_min_rejects_bool(library: TrackLibrary):
    """``True`` is not 1.0 — boolean coercion would silently misfire."""
    _seed(library)
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "library.search_tracks",
            {"query": "", "bpm_min": True},
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_search_compatible_with_compounds_with_query_substring(
    library: TrackLibrary,
):
    """Compat filter + free-text query AND together (post-filter on text)."""
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.search_tracks",
        {"query": "char", "compatible_with_track_id": "alpha"},
    )
    # Only charlie matches "char" substring AND ≤2 from 8B (charlie=9B).
    assert [t["id"] for t in result["tracks"]] == ["charlie"]


# ---- add_track -------------------------------------------------------


@_asyncio
async def test_add_track_validates_path_missing(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "library.add_track", {"path": "/does/not/exist.mp3"}
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_add_track_requires_path_param(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch("library.add_track", {})
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_add_track_runs_analyzer_and_returns_ref(
    library: TrackLibrary,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    fake_track = TrackRef(
        track_id="fake",
        path=str(tmp_path / "fake.mp3"),
        bpm=124.0,
        camelot_key="8B",
        energy=0.21,
        duration_s=200.0,
        beat_grid_anchor_ms=0,
        beat_period_ms=483.87,
        downbeats_ms=[0, 1935, 3870],
    )
    (tmp_path / "fake.mp3").write_bytes(b"\x00")
    monkeypatch.setattr(
        library,
        "add_track_from_path",
        lambda *a, **kw: (library.add_track(fake_track), fake_track)[1],
    )
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.add_track", {"path": str(tmp_path / "fake.mp3")}
    )
    assert result["track"]["id"] == "fake"
    assert result["track"]["camelot_key"] == "8B"
    assert result["track"]["downbeats_ms"] == [0, 1935, 3870]


# ---- add_track_from_directory --------------------------------------


@_asyncio
async def test_add_track_from_directory_scans_and_persists(
    library: TrackLibrary,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    (tmp_path / "song1.mp3").write_bytes(b"\x00")
    (tmp_path / "song2.mp3").write_bytes(b"\x00")
    (tmp_path / "notes.txt").write_text("ignore me")

    def fake_analyzer(path, **_):
        ref = TrackRef(
            track_id=Path(path).stem,
            path=str(path),
            bpm=120.0,
            camelot_key="8B",
            energy=0.2,
            duration_s=180.0,
        )
        library.add_track(ref)
        return ref

    monkeypatch.setattr(library, "add_track_from_path", fake_analyzer)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.add_track_from_directory", {"path": str(tmp_path)}
    )
    assert result["added_count"] == 2
    assert {t["id"] for t in result["added"]} == {"song1", "song2"}
    assert result["total"] == 2


@_asyncio
async def test_add_track_from_directory_rejects_missing(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "library.add_track_from_directory", {"path": "/no/such/dir"}
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


# ---- shape / wire helpers ------------------------------------------


def test_track_ref_to_wire_renames_id_field() -> None:
    ref = TrackRef(
        "kid_cudi-day_n_night",
        "/m/cudi.mp3",
        128.0,
        "8A",
        0.3,
        240.0,
        beat_period_ms=60_000.0 / 128.0,
    )
    wire = track_ref_to_wire(ref)
    assert wire["id"] == "kid_cudi-day_n_night"
    assert "track_id" not in wire
    assert wire["downbeats_ms"] == []
    assert wire["beat_period_ms"] == pytest.approx(60_000.0 / 128.0)


def test_handler_handles_and_namespace_match() -> None:
    handler = LibraryRpcHandler.__new__(LibraryRpcHandler)
    handler._library = None  # type: ignore[attr-defined]
    assert handler.handles("library.list_tracks")
    assert handler.handles("library.search_tracks")
    assert handler.handles("library.set_hot_cues")
    assert not handler.handles("engine.list_effects")
    assert handler.NAMESPACE == "library"


# ---- set_hot_cues (hot-cue persistence PR) -------------------------


@_asyncio
async def test_set_hot_cues_persists_array_and_returns_track(
    library: TrackLibrary,
):
    _seed(library)
    handler = LibraryRpcHandler(library)
    cues = [0, 1500, None, 8000, None, None, 60_000, None]
    result = await handler.dispatch(
        "library.set_hot_cues",
        {"track_id": "alpha", "hot_cues": cues},
    )
    assert result["track"]["id"] == "alpha"
    assert result["track"]["hot_cues"] == cues
    # Read-back via the library confirms persistence (not just echo).
    fetched = library.get("alpha")
    assert fetched is not None
    assert fetched.hot_cues == cues


@_asyncio
async def test_set_hot_cues_unknown_track_id_returns_invalid_params(
    library: TrackLibrary,
):
    _seed(library)
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "library.set_hot_cues",
            {"track_id": "does-not-exist", "hot_cues": [None] * 8},
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS
    assert "not found" in str(exc.value).lower()


@_asyncio
async def test_set_hot_cues_rejects_wrong_length(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "library.set_hot_cues",
            {"track_id": "alpha", "hot_cues": [None, None, None]},  # only 3
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_set_hot_cues_rejects_non_list(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "library.set_hot_cues",
            {"track_id": "alpha", "hot_cues": "not-a-list"},
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_set_hot_cues_rejects_negative_position(library: TrackLibrary):
    _seed(library)
    handler = LibraryRpcHandler(library)
    cues = [None, None, -5, None, None, None, None, None]
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "library.set_hot_cues",
            {"track_id": "alpha", "hot_cues": cues},
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_set_hot_cues_rejects_bool_value(library: TrackLibrary):
    """bool subclasses int in Python — would silently become 0/1 ms."""
    _seed(library)
    handler = LibraryRpcHandler(library)
    cues = [True, None, None, None, None, None, None, None]
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "library.set_hot_cues",
            {"track_id": "alpha", "hot_cues": cues},
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


# ---- hot_cues field on list / search responses ---------------------


@_asyncio
async def test_list_tracks_includes_hot_cues_field(library: TrackLibrary):
    """Every TrackRef in list_tracks must carry an 8-slot hot_cues array."""
    _seed(library)
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.list_tracks", {})
    for t in result["tracks"]:
        assert "hot_cues" in t
        assert isinstance(t["hot_cues"], list)
        assert len(t["hot_cues"]) == 8
        # Default = all-None for tracks that haven't had cues set.
        assert all(c is None for c in t["hot_cues"])


@_asyncio
async def test_search_tracks_includes_hot_cues_after_set(
    library: TrackLibrary,
):
    """set_hot_cues persistence must surface through search results."""
    _seed(library)
    handler = LibraryRpcHandler(library)
    cues = [100, 200, None, None, None, None, None, 99_999]
    await handler.dispatch(
        "library.set_hot_cues",
        {"track_id": "alpha", "hot_cues": cues},
    )
    result = await handler.dispatch(
        "library.search_tracks", {"query": "alpha"}
    )
    assert [t["id"] for t in result["tracks"]] == ["alpha"]
    assert result["tracks"][0]["hot_cues"] == cues


# ---- sync_status (#102 follow-up) ----------------------------------


@_asyncio
async def test_sync_status_empty_library(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.sync_status", {})
    assert result["pending_push_count"] == 0
    assert result["library_track_count"] == 0
    # Daemon stats default to zero when no daemon is wired.
    assert result["last_pull_micros"] == 0
    assert result["last_push_micros"] == 0
    assert result["last_tick_error"] == ""
    assert result["next_sync_micros"] == 0


@_asyncio
async def test_sync_status_includes_daemon_stats_when_wired(
    library: TrackLibrary,
):
    """When the service wires a SyncDaemon, sync_status surfaces the
    last-tick counters so the UI can render "last synced X ago".
    """
    from copilot.cloud_sync import SyncStats

    class StubDaemon:
        def stats(self) -> SyncStats:
            return SyncStats(
                last_pull_micros=1_700_000_000_000_000,
                last_push_micros=1_700_000_000_000_001,
                last_pull_fetched=7,
                last_pull_applied=5,
                last_push_pushed=3,
                last_tick_error="",
                next_sync_micros=1_700_000_060_000_000,
            )

    handler = LibraryRpcHandler(library, sync_daemon=StubDaemon())
    result = await handler.dispatch("library.sync_status", {})
    assert result["last_pull_micros"] == 1_700_000_000_000_000
    assert result["last_pull_fetched"] == 7
    assert result["last_pull_applied"] == 5
    assert result["last_push_pushed"] == 3
    assert result["last_tick_error"] == ""
    assert result["next_sync_micros"] == 1_700_000_060_000_000


@_asyncio
async def test_sync_status_after_local_adds(library: TrackLibrary):
    _seed(library)  # 5 add_track calls → 5 pending push entries.
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.sync_status", {})
    assert result["library_track_count"] == 5
    assert result["pending_push_count"] == 5


@_asyncio
async def test_sync_status_after_clear_pending_push(library: TrackLibrary):
    _seed(library)
    library.clear_pending_push("alpha")
    library.clear_pending_push("bravo")
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.sync_status", {})
    assert result["library_track_count"] == 5
    assert result["pending_push_count"] == 3


def test_handler_handles_sync_status(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    assert handler.handles("library.sync_status") is True


# ---- sync_now (operator-driven force tick) ----------------------------


def test_handler_handles_sync_now(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    assert handler.handles("library.sync_now") is True


@_asyncio
async def test_sync_now_without_daemon_raises(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as ei:
        await handler.dispatch("library.sync_now", {})
    # ``-32000`` (feature-not-installed) matches every other "cloud
    # feature isn't wired" arm of this handler, so the UI can render
    # the same toast for all of them.
    assert ei.value.code == -32000


@_asyncio
async def test_sync_now_calls_daemon_tick_once_and_returns_status(
    library: TrackLibrary,
):
    from copilot.cloud_sync import SyncStats

    class StubDaemon:
        def __init__(self) -> None:
            self.tick_calls = 0
            self._stats = SyncStats()

        def tick_once(self) -> None:
            self.tick_calls += 1
            # Mutate stats so the post-tick status reflects the tick.
            self._stats = SyncStats(
                last_pull_micros=1_700_000_000_000_000,
                last_push_micros=1_700_000_000_000_000,
                last_pull_fetched=4,
                last_pull_applied=4,
                last_push_pushed=2,
                last_tick_error="",
            )

        def stats(self) -> SyncStats:
            return self._stats

    daemon = StubDaemon()
    handler = LibraryRpcHandler(library, sync_daemon=daemon)
    result = await handler.dispatch("library.sync_now", {})
    assert daemon.tick_calls == 1
    assert result["last_pull_micros"] == 1_700_000_000_000_000
    assert result["last_pull_fetched"] == 4
    assert result["last_push_pushed"] == 2
    assert result["last_tick_error"] == ""


@_asyncio
async def test_sync_now_calls_wake_now_after_tick(library: TrackLibrary):
    """After running an out-of-band tick, the RPC must kick the
    daemon's `_wake` so its next automatic tick fires at the now-
    reset cadence instead of finishing the prior backoff wait.
    """
    from copilot.cloud_sync import SyncStats

    class StubDaemon:
        def __init__(self) -> None:
            self.tick_calls = 0
            self.wake_calls = 0
            self._stats = SyncStats()

        def tick_once(self) -> None:
            self.tick_calls += 1

        def wake_now(self) -> None:
            self.wake_calls += 1

        def stats(self) -> SyncStats:
            return self._stats

    daemon = StubDaemon()
    handler = LibraryRpcHandler(library, sync_daemon=daemon)
    await handler.dispatch("library.sync_now", {})
    assert daemon.tick_calls == 1
    assert daemon.wake_calls == 1, (
        "sync_now must call wake_now to refresh the daemon's schedule"
    )


@_asyncio
async def test_sync_now_tolerates_daemon_without_wake_now(
    library: TrackLibrary,
):
    """Older daemon stubs (and older deployed copilots during an
    in-place upgrade) may not expose `wake_now`. The RPC must
    tolerate that — `wake_now` is best-effort."""
    from copilot.cloud_sync import SyncStats

    class OldStubDaemon:
        def tick_once(self) -> None:
            pass

        def stats(self) -> SyncStats:
            return SyncStats()

        # No wake_now method.

    handler = LibraryRpcHandler(library, sync_daemon=OldStubDaemon())
    # Must not raise AttributeError.
    await handler.dispatch("library.sync_now", {})


@_asyncio
async def test_sync_now_wraps_transport_error_as_rpc_error(
    library: TrackLibrary,
):
    from copilot.cloud_sync.client import SyncError

    class FlakyDaemon:
        def tick_once(self) -> None:
            raise SyncError("HTTP 503")

    handler = LibraryRpcHandler(library, sync_daemon=FlakyDaemon())
    with pytest.raises(RpcError) as ei:
        await handler.dispatch("library.sync_now", {})
    # Internal error — the surface is "we tried, the cloud said no".
    assert ei.value.code == -32603
    assert "HTTP 503" in str(ei.value)


# ---- list_pending_push --------------------------------------------


def test_handler_handles_list_pending_push(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    assert handler.handles("library.list_pending_push") is True


@_asyncio
async def test_list_pending_push_empty(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.list_pending_push", {})
    assert result == {"ids": []}


@_asyncio
async def test_list_pending_push_after_adds(library: TrackLibrary):
    _seed(library)  # 5 add_track calls → 5 pending push entries.
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.list_pending_push", {})
    assert sorted(result["ids"]) == [
        "alpha",
        "bravo",
        "charlie",
        "delta",
        "echo",
    ]


@_asyncio
async def test_list_pending_push_after_partial_clear(
    library: TrackLibrary,
):
    _seed(library)
    library.clear_pending_push("alpha")
    library.clear_pending_push("delta")
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.list_pending_push", {})
    assert sorted(result["ids"]) == ["bravo", "charlie", "echo"]


# ---- requeue_all_pending --------------------------------------------


def test_handler_handles_requeue_all_pending(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    assert handler.handles("library.requeue_all_pending") is True


@_asyncio
async def test_requeue_all_pending_empty_library(library: TrackLibrary):
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.requeue_all_pending", {})
    assert result == {"queued": 0}


@_asyncio
async def test_requeue_all_pending_seeds_pre_cloud_sync_library(
    library: TrackLibrary,
):
    """Operator escape hatch — after a pre-cloud-sync upgrade the
    local library has tracks but no pending_push rows. The RPC must
    enqueue everything."""
    _seed(library)  # 5 adds → 5 auto-enqueued rows
    for tid in ["alpha", "bravo", "charlie", "delta", "echo"]:
        library.clear_pending_push(tid)
    assert library.pending_push_ids() == []
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch("library.requeue_all_pending", {})
    assert result == {"queued": 5}
    assert sorted(library.pending_push_ids()) == [
        "alpha",
        "bravo",
        "charlie",
        "delta",
        "echo",
    ]


@_asyncio
async def test_sync_now_wraps_sqlite_error_as_rpc_error(
    library: TrackLibrary,
):
    import sqlite3 as _sqlite3

    class LockedDaemon:
        def tick_once(self) -> None:
            raise _sqlite3.OperationalError("database is locked")

    handler = LibraryRpcHandler(library, sync_daemon=LockedDaemon())
    with pytest.raises(RpcError) as ei:
        await handler.dispatch("library.sync_now", {})
    assert ei.value.code == -32603
    assert "database is locked" in str(ei.value)


