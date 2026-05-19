"""Outbound push queue tests (#102 slice 5)."""

from __future__ import annotations

from pathlib import Path

import pytest

from copilot.cloud_sync import (
    InMemorySyncClient,
    LibrarySyncer,
    PushResult,
    RemoteTrack,
    SyncError,
)
from copilot.cloud_sync.bootstrap import bootstrap_push
from copilot.library import TrackLibrary, TrackRef


def _make_track(track_id: str) -> TrackRef:
    return TrackRef(
        track_id=track_id,
        path=f"/tmp/{track_id}.mp3",
        bpm=120.0,
        camelot_key="8B",
        energy=0.5,
        duration_s=200.0,
    )


# ----- queue table -------------------------------------------------


def test_add_track_enqueues_pending_push(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        lib.add_track(_make_track("t1"))
        assert lib.pending_push_ids() == ["t1"]
    finally:
        lib.close()


def test_upsert_from_remote_does_not_enqueue(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        lib.upsert_from_remote(
            track_id="cloud-1",
            path="/c/a.mp3",
            bpm=120.0,
            camelot_key="8B",
            energy=0.5,
            duration_s=100.0,
            hot_cues=[None] * 8,
            updated_at_micros=42,
        )
        # Remote-sourced row must NOT bounce back to the cloud.
        assert lib.pending_push_ids() == []
    finally:
        lib.close()


def test_clear_pending_push(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        lib.add_track(_make_track("t1"))
        lib.add_track(_make_track("t2"))
        assert set(lib.pending_push_ids()) == {"t1", "t2"}
        lib.clear_pending_push("t1")
        assert lib.pending_push_ids() == ["t2"]
        # Clearing a non-queued id is a no-op.
        lib.clear_pending_push("never-queued")
        assert lib.pending_push_ids() == ["t2"]
    finally:
        lib.close()


def test_row_for_cloud_push_returns_seven_tuple(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        lib.add_track(_make_track("t"))
        row = lib.row_for_cloud_push("t")
        assert row is not None
        path, bpm, key, energy, duration_s, hot_cues, ts = row
        assert path == "/tmp/t.mp3"
        assert bpm == 120.0
        assert key == "8B"
        assert isinstance(hot_cues, list) and len(hot_cues) == 8
        assert ts > 0
    finally:
        lib.close()


def test_row_for_cloud_push_returns_none_for_unknown(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        assert lib.row_for_cloud_push("nope") is None
    finally:
        lib.close()


# ----- LibrarySyncer.push_pending ----------------------------------


def test_push_pending_empties_queue_on_success(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    client = InMemorySyncClient()
    try:
        lib.add_track(_make_track("t1"))
        lib.add_track(_make_track("t2"))
        syncer = LibrarySyncer(
            client,
            local_updated_at=lambda _id: None,
            apply_remote=lambda _row: None,
        )
        result = syncer.push_pending(
            lib.pending_push_ids(),
            row_loader=lib.row_for_cloud_push,
            on_pushed=lib.clear_pending_push,
        )
        assert result.pushed_count == 2
        assert result.skipped_missing_count == 0
        assert result.transport_error is None
        assert lib.pending_push_ids() == []
        # And the rows are now in the remote.
        assert len(client) == 2
    finally:
        lib.close()


def test_push_pending_skips_deleted_rows(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    client = InMemorySyncClient()
    try:
        lib.add_track(_make_track("ghost"))
        # Simulate a TOCTOU window where the row was deleted between
        # enqueue + flush. We can't easily delete in the public API
        # without churn, so monkey-patch row_for_cloud_push to return
        # None for the one id.
        original = lib.row_for_cloud_push

        def _load(track_id: str):
            if track_id == "ghost":
                return None
            return original(track_id)

        syncer = LibrarySyncer(
            client,
            local_updated_at=lambda _id: None,
            apply_remote=lambda _row: None,
        )
        result = syncer.push_pending(
            ["ghost"],
            row_loader=_load,
            on_pushed=lib.clear_pending_push,
        )
        assert result.pushed_count == 0
        assert result.skipped_missing_count == 1
    finally:
        lib.close()


def test_push_pending_halts_on_transport_error(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        lib.add_track(_make_track("t1"))
        lib.add_track(_make_track("t2"))

        class BoomClient:
            def list_tracks(self, *, since_micros: int = 0):
                return []

            def get_track(self, _id):
                return None

            def upsert_track(self, _t):
                raise SyncError("boom")

            def delete_track(self, _id):
                return False

        syncer = LibrarySyncer(
            BoomClient(),
            local_updated_at=lambda _id: None,
            apply_remote=lambda _row: None,
        )
        result = syncer.push_pending(
            lib.pending_push_ids(),
            row_loader=lib.row_for_cloud_push,
            on_pushed=lib.clear_pending_push,
        )
        assert result.pushed_count == 0
        assert result.transport_error == "boom"
        # Queue stays intact for retry.
        assert sorted(lib.pending_push_ids()) == ["t1", "t2"]
    finally:
        lib.close()


def test_push_pending_succeeds_then_remote_can_be_re_read(tmp_path: Path) -> None:
    """Round-trip — local add → push → remote get returns the row."""
    lib = TrackLibrary(tmp_path / "lib.db")
    client = InMemorySyncClient()
    try:
        lib.add_track(_make_track("rt"))
        syncer = LibrarySyncer(
            client,
            local_updated_at=lambda _id: None,
            apply_remote=lambda _row: None,
        )
        syncer.push_pending(
            lib.pending_push_ids(),
            row_loader=lib.row_for_cloud_push,
            on_pushed=lib.clear_pending_push,
        )
        remote = client.get_track("rt")
        assert remote is not None
        assert remote.path == "/tmp/rt.mp3"
        assert remote.bpm == 120.0
    finally:
        lib.close()


# ----- bootstrap_push --------------------------------------------------


def test_bootstrap_push_empty_queue_is_a_noop(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    client = InMemorySyncClient()
    try:
        result = bootstrap_push(client, lib)
        assert result.pushed_count == 0
        assert result.transport_error is None
    finally:
        lib.close()


def test_bootstrap_push_drains_after_pull_round_trip(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    client = InMemorySyncClient()
    try:
        lib.add_track(_make_track("local-1"))
        result = bootstrap_push(client, lib)
        assert result.pushed_count == 1
        # Queue now empty.
        assert lib.pending_push_ids() == []
        # Remote got it.
        assert client.get_track("local-1") is not None
    finally:
        lib.close()
