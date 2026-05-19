"""PlaylistQueue + PlaylistRpcHandler tests.

The queue is small but the integration points are subtle:
* dequeue must skip dangling track ids (library row was removed),
* reorder must clamp out-of-range positions,
* persistence must survive a process restart (== a fresh
  ``TrackLibrary(":memory:")`` won't — that's per-test; we exercise
  durability via a tmp file path),
* the auto-mix controller must consume from the queue *before*
  falling back to the mashability ranker.
"""
from __future__ import annotations

import asyncio
from pathlib import Path

import pytest

from copilot.auto_mix import AutoMixController
from copilot.library import TrackLibrary, TrackRef
from copilot.library_rpc import RpcError
from copilot.playlist import PlaylistQueue, entry_to_wire
from copilot.playlist_rpc import PlaylistRpcHandler
from copilot.proposer import TransitionProposer
from copilot.schemas import (
    Deck,
    DeckId,
    EngineState,
    Event,
    TrackRef as EngineTrackRef,
)


# ---------- helpers ----------


def _seed(lib: TrackLibrary) -> None:
    lib.add_track(TrackRef("a", "/a.mp3", 124.0, "8B", 0.20, 210.0))
    lib.add_track(TrackRef("b", "/b.mp3", 125.0, "8B", 0.22, 220.0))
    lib.add_track(TrackRef("c", "/c.mp3", 126.0, "8B", 0.24, 230.0))
    lib.add_track(TrackRef("d", "/d.mp3", 127.0, "8B", 0.26, 240.0))


# ---------- enqueue / dequeue ----------


def test_enqueue_appends_in_order(library: TrackLibrary) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    q.enqueue("b")
    q.enqueue("c")
    ids = [e.track_id for e in q.list_queue()]
    positions = [e.position for e in q.list_queue()]
    assert ids == ["a", "b", "c"]
    assert positions == [0, 1, 2]


def test_dequeue_returns_head_and_shrinks_queue(library: TrackLibrary) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    q.enqueue("b")
    assert q.dequeue() == "a"
    assert [e.track_id for e in q.list_queue()] == ["b"]
    # Positions stay dense (the renumber pass moves "b" to pos 0).
    assert q.list_queue()[0].position == 0
    assert q.dequeue() == "b"
    assert q.dequeue() is None


def test_dequeue_skips_dangling_track_ids(library: TrackLibrary) -> None:
    """Queue entries pointing at a track id that's gone from the library
    are silently skipped on dequeue so the auto-mix controller never
    sees a stale id."""
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    q.enqueue("b")
    # Simulate operator deleting "a" from the library mid-set.
    library._conn.execute("DELETE FROM tracks WHERE track_id = 'a'")  # noqa: SLF001
    library._conn.commit()  # noqa: SLF001
    assert q.dequeue() == "b"  # "a" skipped, "b" returned.


# ---------- reorder ----------


def test_reorder_moves_track_within_queue(library: TrackLibrary) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    q.enqueue("b")
    q.enqueue("c")
    q.reorder("c", 0)
    assert [e.track_id for e in q.list_queue()] == ["c", "a", "b"]
    assert [e.position for e in q.list_queue()] == [0, 1, 2]


def test_reorder_clamps_out_of_range_positions(
    library: TrackLibrary,
) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    q.enqueue("b")
    q.enqueue("c")
    # Negative -> snap to 0.
    q.reorder("c", -5)
    assert [e.track_id for e in q.list_queue()] == ["c", "a", "b"]
    # Overshoot -> snap to len - 1.
    q.reorder("c", 99)
    assert [e.track_id for e in q.list_queue()] == ["a", "b", "c"]


def test_reorder_unknown_track_raises_keyerror(
    library: TrackLibrary,
) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    with pytest.raises(KeyError):
        q.reorder("missing", 0)


# ---------- remove + clear ----------


def test_remove_drops_track_and_renumbers(library: TrackLibrary) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    q.enqueue("b")
    q.enqueue("c")
    q.remove("b")
    assert [e.track_id for e in q.list_queue()] == ["a", "c"]
    assert [e.position for e in q.list_queue()] == [0, 1]


def test_remove_unknown_raises_keyerror(library: TrackLibrary) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    with pytest.raises(KeyError):
        q.remove("nothing-here")


def test_clear_empties_queue(library: TrackLibrary) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    q.enqueue("b")
    q.clear()
    assert q.list_queue() == []
    # Idempotent — clearing an empty queue is fine.
    q.clear()
    assert q.list_queue() == []


# ---------- persistence ----------


def test_queue_survives_library_reopen(tmp_path: Path) -> None:
    """Queue state persists across :class:`TrackLibrary` instances when
    backed by a real file (== process restart equivalence)."""
    db_path = tmp_path / "library.db"
    lib1 = TrackLibrary(db_path)
    _seed(lib1)
    q1 = PlaylistQueue(lib1)
    q1.enqueue("a")
    q1.enqueue("b")
    lib1.close()

    lib2 = TrackLibrary(db_path)
    try:
        q2 = PlaylistQueue(lib2)
        assert [e.track_id for e in q2.list_queue()] == ["a", "b"]
    finally:
        lib2.close()


# ---------- RPC handler wire shape ----------


async def test_rpc_handler_dispatches_full_surface(
    library: TrackLibrary,
) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    handler = PlaylistRpcHandler(q)

    # enqueue + list round-trip
    enq = await handler.dispatch("playlist.enqueue", {"track_id": "a"})
    assert enq["entry"]["track_id"] == "a"
    assert enq["entry"]["position"] == 0
    assert enq["entry"]["track"]["id"] == "a"

    await handler.dispatch("playlist.enqueue", {"track_id": "b"})
    listed = await handler.dispatch("playlist.list", {})
    assert [e["track_id"] for e in listed["entries"]] == ["a", "b"]

    # reorder
    reordered = await handler.dispatch(
        "playlist.reorder", {"track_id": "b", "new_position": 0}
    )
    assert [e["track_id"] for e in reordered["entries"]] == ["b", "a"]

    # remove
    removed = await handler.dispatch(
        "playlist.remove", {"track_id": "a"}
    )
    assert [e["track_id"] for e in removed["entries"]] == ["b"]

    # clear
    cleared = await handler.dispatch("playlist.clear", {})
    assert cleared == {"ok": True}


async def test_rpc_handler_rejects_bad_params(
    library: TrackLibrary,
) -> None:
    _seed(library)
    q = PlaylistQueue(library)
    handler = PlaylistRpcHandler(q)
    # Empty track id -> -32602.
    with pytest.raises(RpcError):
        await handler.dispatch("playlist.enqueue", {"track_id": ""})
    # Missing new_position.
    q.enqueue("a")
    with pytest.raises(RpcError):
        await handler.dispatch(
            "playlist.reorder", {"track_id": "a"}
        )
    # Removing a track that isn't in the queue.
    with pytest.raises(RpcError):
        await handler.dispatch(
            "playlist.remove", {"track_id": "ghost"}
        )


def test_entry_to_wire_handles_missing_track(library: TrackLibrary) -> None:
    """A dangling entry surfaces as ``track: null`` so the UI can show
    the entry with a 'missing' badge instead of swallowing it."""
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("a")
    # Yank "a" from the library after enqueue.
    library._conn.execute("DELETE FROM tracks WHERE track_id = 'a'")  # noqa: SLF001
    library._conn.commit()  # noqa: SLF001
    entries = q.list_queue()
    wire = entry_to_wire(entries[0])
    assert wire["track_id"] == "a"
    assert wire["track"] is None


# ---------- auto-mix integration ----------


def _near_end_state() -> EngineState:
    return EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="a", path="/a.mp3"),
            playing=True,
            position_ms=190_000,
            copilot_engaged=True,
            bpm=124.0,
        ),
        deck_b=Deck(copilot_engaged=True),
        session_active=True,
    )


def _make_controller(
    library: TrackLibrary, playlist: PlaylistQueue | None
) -> tuple[AutoMixController, list[Event]]:
    submitted: list[Event] = []

    async def submit(ev: Event) -> None:
        submitted.append(ev)

    proposer = TransitionProposer(library)
    ctrl = AutoMixController(
        proposer,
        submit,
        state_changed=None,
        playlist=playlist,
    )
    return ctrl, submitted


async def test_auto_mix_consumes_playlist_head_when_queue_non_empty(
    library: TrackLibrary,
) -> None:
    """Auto-mix prefers the playlist head over the mashability ranker.

    Seeded library: the ranker would pick "b" (closest BPM / same key
    / closest energy to "a"). With the playlist holding ["d", "c"],
    auto-mix must load "d" first — the operator's explicit pick wins.
    """
    _seed(library)
    q = PlaylistQueue(library)
    q.enqueue("d")
    q.enqueue("c")
    ctrl, submitted = _make_controller(library, q)
    ctrl.set_auto_mix(DeckId.A, True)

    await ctrl.tick(_near_end_state())
    for _ in range(5):
        await asyncio.sleep(0)

    # DeckLoad on the target deck must point at "d" (the queue head),
    # not "b" (the mashability ranker's pick).
    assert len(submitted) > 0
    first = submitted[0].kind.model_dump()
    assert first["kind"] == "DeckLoad"
    assert first["track"]["id"] == "d"
    # Queue has been popped — only "c" remains.
    assert [e.track_id for e in q.list_queue()] == ["c"]


async def test_auto_mix_falls_back_to_ranker_when_queue_empty(
    library: TrackLibrary,
) -> None:
    """An empty queue must trigger the legacy mashability flow exactly
    (== this PR can't regress the no-playlist path)."""
    _seed(library)
    q = PlaylistQueue(library)  # empty queue
    ctrl, submitted = _make_controller(library, q)
    ctrl.set_auto_mix(DeckId.A, True)

    await ctrl.tick(_near_end_state())
    for _ in range(5):
        await asyncio.sleep(0)

    assert len(submitted) > 0
    first = submitted[0].kind.model_dump()
    assert first["kind"] == "DeckLoad"
    # "b" is the obvious-best ranker pick for the seeded library.
    assert first["track"]["id"] == "b"
    # Queue stays empty.
    assert q.list_queue() == []


async def test_auto_mix_without_playlist_arg_preserves_legacy_path(
    library: TrackLibrary,
) -> None:
    """Constructing the controller without ``playlist=`` keeps the
    pre-PR behaviour exactly. Existing tests already cover this, but
    we double-check here so the playlist wiring can never silently
    change the contract for legacy callers."""
    _seed(library)
    ctrl, submitted = _make_controller(library, playlist=None)
    ctrl.set_auto_mix(DeckId.A, True)
    await ctrl.tick(_near_end_state())
    for _ in range(5):
        await asyncio.sleep(0)
    assert len(submitted) > 0
    first = submitted[0].kind.model_dump()
    assert first["track"]["id"] == "b"
