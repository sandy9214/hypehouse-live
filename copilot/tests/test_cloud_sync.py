"""Tests for the cloud_sync scaffold (issue #102 slice 1)."""

from __future__ import annotations

import pytest

from copilot.cloud_sync import (
    ConflictOutcome,
    InMemorySyncClient,
    LibrarySyncer,
    RemoteTrack,
    SyncError,
)


def _mk(track_id: str, *, updated_at: int = 0, path: str = "/tmp/t.mp3") -> RemoteTrack:
    return RemoteTrack(
        track_id=track_id,
        path=path,
        bpm=120.0,
        camelot_key="8B",
        energy=0.5,
        duration_s=200.0,
        updated_at_micros=updated_at,
    )


# -------- RemoteTrack codec ---------------------------------------


def test_hot_cues_as_options_decodes_minus_one_to_none() -> None:
    r = RemoteTrack(
        track_id="t",
        path="/x",
        bpm=120.0,
        camelot_key="8B",
        energy=0.5,
        duration_s=100.0,
        hot_cues_ms=(1000, -1, 5000, -1, -1, -1, -1, -1),
    )
    assert r.hot_cues_as_options() == [1000, None, 5000, None, None, None, None, None]


def test_from_local_pads_and_encodes_none_to_minus_one() -> None:
    r = RemoteTrack.from_local(
        track_id="t",
        path="/x",
        bpm=120.0,
        camelot_key="8B",
        energy=0.5,
        duration_s=100.0,
        hot_cues=[1000, None, 2000],  # length 3 — should pad to 8
        updated_at_micros=42,
    )
    assert len(r.hot_cues_ms) == 8
    assert r.hot_cues_ms[0] == 1000
    assert r.hot_cues_ms[1] == -1
    assert r.hot_cues_ms[2] == 2000
    assert r.hot_cues_ms[3:] == (-1, -1, -1, -1, -1)
    assert r.updated_at_micros == 42


def test_from_local_truncates_over_8_slots() -> None:
    r = RemoteTrack.from_local(
        track_id="t",
        path="/x",
        bpm=120.0,
        camelot_key="8B",
        energy=0.5,
        duration_s=100.0,
        hot_cues=[i for i in range(20)],
        updated_at_micros=0,
    )
    assert len(r.hot_cues_ms) == 8
    assert r.hot_cues_ms == (0, 1, 2, 3, 4, 5, 6, 7)


# -------- InMemorySyncClient -------------------------------------


def test_in_memory_seed_and_list() -> None:
    client = InMemorySyncClient(seed=[_mk("a", updated_at=10), _mk("b", updated_at=20)])
    assert len(client) == 2
    rows = client.list_tracks()
    assert {r.track_id for r in rows} == {"a", "b"}


def test_in_memory_list_filters_since_micros() -> None:
    client = InMemorySyncClient(seed=[_mk("old", updated_at=5), _mk("new", updated_at=15)])
    fresh = client.list_tracks(since_micros=10)
    assert {r.track_id for r in fresh} == {"new"}


def test_in_memory_upsert_last_write_wins() -> None:
    client = InMemorySyncClient()
    client.upsert_track(_mk("a", updated_at=10, path="/v1"))
    client.upsert_track(_mk("a", updated_at=20, path="/v2"))
    assert client.get_track("a").path == "/v2"
    # Older write must NOT clobber newer one.
    client.upsert_track(_mk("a", updated_at=15, path="/v_stale"))
    assert client.get_track("a").path == "/v2"


def test_in_memory_delete() -> None:
    client = InMemorySyncClient(seed=[_mk("a")])
    assert client.delete_track("a") is True
    assert client.delete_track("a") is False
    assert client.get_track("a") is None


# -------- LibrarySyncer --------------------------------------------


def test_pull_inserts_when_local_missing() -> None:
    remote = InMemorySyncClient(seed=[_mk("a", updated_at=100)])
    applied: list[RemoteTrack] = []
    syncer = LibrarySyncer(
        remote,
        local_updated_at=lambda _id: None,
        apply_remote=applied.append,
    )
    result = syncer.pull()
    assert result.fetched_count == 1
    assert result.inserted_count == 1
    assert result.applied_count == 0
    assert result.kept_local_count == 0
    assert result.track_outcomes["a"] == ConflictOutcome.LOCAL_INSERTED
    assert len(applied) == 1
    assert applied[0].track_id == "a"


def test_pull_applies_remote_when_newer() -> None:
    remote = InMemorySyncClient(seed=[_mk("a", updated_at=200)])
    applied: list[RemoteTrack] = []
    syncer = LibrarySyncer(
        remote,
        local_updated_at=lambda _id: 100,
        apply_remote=applied.append,
    )
    result = syncer.pull()
    assert result.applied_count == 1
    assert result.kept_local_count == 0
    assert result.track_outcomes["a"] == ConflictOutcome.REMOTE_APPLIED
    assert len(applied) == 1


def test_pull_keeps_local_when_local_is_newer() -> None:
    remote = InMemorySyncClient(seed=[_mk("a", updated_at=50)])
    applied: list[RemoteTrack] = []
    syncer = LibrarySyncer(
        remote,
        local_updated_at=lambda _id: 100,
        apply_remote=applied.append,
    )
    result = syncer.pull()
    assert result.applied_count == 0
    assert result.kept_local_count == 1
    assert result.inserted_count == 0
    assert result.track_outcomes["a"] == ConflictOutcome.LOCAL_KEPT
    assert applied == []  # callback NOT invoked when local wins


def test_pull_mixed_inserts_applies_keeps_in_one_pass() -> None:
    remote = InMemorySyncClient(
        seed=[
            _mk("ins", updated_at=10),
            _mk("upd", updated_at=200),
            _mk("keep", updated_at=50),
        ]
    )
    local_versions = {"upd": 100, "keep": 100}
    syncer = LibrarySyncer(
        remote,
        local_updated_at=lambda tid: local_versions.get(tid),
        apply_remote=lambda _row: None,
    )
    result = syncer.pull()
    assert result.fetched_count == 3
    assert result.inserted_count == 1
    assert result.applied_count == 1
    assert result.kept_local_count == 1


def test_pull_captures_transport_error_without_raising() -> None:
    class BoomClient:
        def list_tracks(self, *, since_micros: int = 0):
            raise SyncError("network down")

        def get_track(self, _id):  # unused; protocol surface only.
            raise NotImplementedError

        def upsert_track(self, _t):
            raise NotImplementedError

        def delete_track(self, _id):
            raise NotImplementedError

    syncer = LibrarySyncer(
        BoomClient(),
        local_updated_at=lambda _id: None,
        apply_remote=lambda _row: None,
    )
    result = syncer.pull()
    assert result.fetched_count == 0
    assert result.transport_error == "network down"


def test_pull_skips_apply_callback_when_local_wins() -> None:
    """Regression — callback should NOT fire when local row is newer."""
    remote = InMemorySyncClient(seed=[_mk("a", updated_at=50)])
    calls = 0

    def apply(_row: RemoteTrack) -> None:
        nonlocal calls
        calls += 1

    syncer = LibrarySyncer(
        remote,
        local_updated_at=lambda _id: 1_000,
        apply_remote=apply,
    )
    result = syncer.pull()
    assert calls == 0
    assert result.kept_local_count == 1


def test_queue_local_change_is_a_noop_today() -> None:
    """Slice 1 stubs the outbound push queue. Don't error."""
    syncer = LibrarySyncer(
        InMemorySyncClient(),
        local_updated_at=lambda _id: None,
        apply_remote=lambda _row: None,
    )
    syncer.queue_local_change(_mk("a"))  # must not raise.


def test_pull_with_since_micros_filters_at_client_level() -> None:
    remote = InMemorySyncClient(seed=[_mk("old", updated_at=5), _mk("new", updated_at=50)])
    applied: list[RemoteTrack] = []
    syncer = LibrarySyncer(
        remote,
        local_updated_at=lambda _id: None,
        apply_remote=applied.append,
    )
    result = syncer.pull(since_micros=20)
    assert result.fetched_count == 1
    assert applied[0].track_id == "new"
