"""TrackLibrary cloud-sync surface tests (#102 slice 4)."""

from __future__ import annotations

from pathlib import Path

from copilot.cloud_sync import InMemorySyncClient, RemoteTrack
from copilot.cloud_sync.bootstrap import bootstrap_pull
from copilot.library import HOT_CUE_SLOTS, TrackLibrary, TrackRef


def _make_track(track_id: str = "t1", path: str = "/tmp/a.mp3") -> TrackRef:
    return TrackRef(
        track_id=track_id,
        path=path,
        bpm=120.0,
        camelot_key="8B",
        energy=0.5,
        duration_s=180.0,
    )


def test_add_track_stamps_updated_at_micros(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        lib.add_track(_make_track("t1"))
        ts = lib.local_updated_at_micros("t1")
        assert ts is not None
        assert ts > 0
    finally:
        lib.close()


def test_local_updated_at_micros_returns_none_for_unknown(
    tmp_path: Path,
) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        assert lib.local_updated_at_micros("nope") is None
    finally:
        lib.close()


def test_upsert_from_remote_preserves_wire_timestamp(
    tmp_path: Path,
) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        lib.upsert_from_remote(
            track_id="cloud-1",
            path="/cloud/a.mp3",
            bpm=124.0,
            camelot_key="9A",
            energy=0.7,
            duration_s=200.0,
            hot_cues=[1000, None, None, None, None, None, None, None],
            updated_at_micros=987_654_321,
        )
        ts = lib.local_updated_at_micros("cloud-1")
        assert ts == 987_654_321
    finally:
        lib.close()


def test_subsequent_local_write_supersedes_remote(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        lib.upsert_from_remote(
            track_id="t",
            path="/cloud/a.mp3",
            bpm=124.0,
            camelot_key="9A",
            energy=0.5,
            duration_s=200.0,
            hot_cues=[None] * HOT_CUE_SLOTS,
            updated_at_micros=500,
        )
        # Local edit stamps with wall-clock micros, which is many
        # orders of magnitude larger than 500 — must reclaim.
        lib.add_track(_make_track("t"))
        ts = lib.local_updated_at_micros("t")
        assert ts is not None and ts > 500
    finally:
        lib.close()


# ----- bootstrap_pull end-to-end with library merge ---------------------


def test_bootstrap_pull_inserts_remote_into_library(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        client = InMemorySyncClient(
            seed=[
                RemoteTrack(
                    track_id="cloud-only",
                    path="/c/a.mp3",
                    bpm=120.0,
                    camelot_key="8B",
                    energy=0.5,
                    duration_s=100.0,
                    updated_at_micros=42_000,
                ),
            ],
        )
        result = bootstrap_pull(client, library=lib)
        assert result.fetched_count == 1
        assert result.inserted_count == 1
        # And the row landed in the library DB.
        assert lib.local_updated_at_micros("cloud-only") == 42_000
    finally:
        lib.close()


def test_bootstrap_pull_keeps_local_when_local_is_newer(tmp_path: Path) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        # Local-only write stamps with wall-clock micros (~10^15).
        lib.add_track(_make_track("t"))
        # Remote row from before the unix epoch — local must win.
        client = InMemorySyncClient(
            seed=[
                RemoteTrack(
                    track_id="t",
                    path="/c/should-be-ignored.mp3",
                    bpm=999.0,
                    camelot_key="ZZ",
                    energy=0.0,
                    duration_s=0.0,
                    updated_at_micros=1,
                ),
            ],
        )
        result = bootstrap_pull(client, library=lib)
        assert result.fetched_count == 1
        assert result.kept_local_count == 1
        assert result.applied_count == 0
        # Path stayed local.
        row = lib.get("t")
        assert row is not None
        assert row.path == "/tmp/a.mp3"
    finally:
        lib.close()


def test_bootstrap_pull_applies_remote_when_remote_is_newer(
    tmp_path: Path,
) -> None:
    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        # Seed local row with a low watermark.
        lib.upsert_from_remote(
            track_id="t",
            path="/local/old.mp3",
            bpm=100.0,
            camelot_key="1A",
            energy=0.3,
            duration_s=150.0,
            hot_cues=[None] * HOT_CUE_SLOTS,
            updated_at_micros=10,
        )
        # Remote has a newer write.
        client = InMemorySyncClient(
            seed=[
                RemoteTrack(
                    track_id="t",
                    path="/cloud/new.mp3",
                    bpm=128.0,
                    camelot_key="5A",
                    energy=0.8,
                    duration_s=240.0,
                    updated_at_micros=10_000,
                ),
            ],
        )
        result = bootstrap_pull(client, library=lib)
        assert result.applied_count == 1
        # Row was overwritten.
        ts = lib.local_updated_at_micros("t")
        assert ts == 10_000
        row = lib.get("t")
        assert row is not None
        assert row.path == "/cloud/new.mp3"
        assert row.bpm == 128.0
    finally:
        lib.close()
