"""Sync daemon tests (#102 slice 6)."""

from __future__ import annotations

import time
from pathlib import Path

import pytest

from copilot.cloud_sync import (
    InMemorySyncClient,
    RemoteTrack,
    SyncDaemon,
)
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


# ----- from_env defaults ------------------------------------------


def test_from_env_default_60s(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.delenv("HYPEHOUSE_SYNC_TICK_SECONDS", raising=False)
    d = SyncDaemon.from_env(InMemorySyncClient(), tmp_path / "lib.db")
    assert d._tick == 60.0  # noqa: SLF001 — exercising public surface via private attr


def test_from_env_reads_custom_tick(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("HYPEHOUSE_SYNC_TICK_SECONDS", "15")
    d = SyncDaemon.from_env(InMemorySyncClient(), tmp_path / "lib.db")
    assert d._tick == 15.0  # noqa: SLF001


def test_from_env_invalid_value_falls_back_to_default(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("HYPEHOUSE_SYNC_TICK_SECONDS", "not-a-number")
    d = SyncDaemon.from_env(InMemorySyncClient(), tmp_path / "lib.db")
    assert d._tick == 60.0  # noqa: SLF001


def test_from_env_zero_or_negative_falls_back_to_default(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("HYPEHOUSE_SYNC_TICK_SECONDS", "-3")
    d = SyncDaemon.from_env(InMemorySyncClient(), tmp_path / "lib.db")
    assert d._tick == 60.0  # noqa: SLF001


def test_constructor_clamps_minimum_tick(tmp_path: Path) -> None:
    # 0.001 → clamped to 0.01 to avoid a busy loop.
    d = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=0.001
    )
    assert d._tick >= 0.01  # noqa: SLF001


# ----- tick_once -------------------------------------------------


def test_tick_once_drains_pending_push(tmp_path: Path) -> None:
    db = tmp_path / "lib.db"
    # Seed a local row needing upstream sync.
    lib = TrackLibrary(db)
    lib.add_track(_make_track("t"))
    assert lib.pending_push_ids() == ["t"]
    lib.close()
    client = InMemorySyncClient()
    daemon = SyncDaemon(client, db, tick_seconds=60)
    daemon.tick_once()
    # Pending queue drained.
    lib2 = TrackLibrary(db)
    try:
        assert lib2.pending_push_ids() == []
    finally:
        lib2.close()
    # And the remote got it.
    assert client.get_track("t") is not None


def test_tick_once_pulls_remote_into_local(tmp_path: Path) -> None:
    db = tmp_path / "lib.db"
    client = InMemorySyncClient(
        seed=[
            RemoteTrack(
                track_id="cloud-only",
                path="/c/a.mp3",
                bpm=125.0,
                camelot_key="9A",
                energy=0.6,
                duration_s=180.0,
                updated_at_micros=1_000,
            ),
        ],
    )
    daemon = SyncDaemon(client, db, tick_seconds=60)
    daemon.tick_once()
    lib = TrackLibrary(db)
    try:
        row = lib.get("cloud-only")
        assert row is not None
        assert row.path == "/c/a.mp3"
        assert lib.local_updated_at_micros("cloud-only") == 1_000
    finally:
        lib.close()


# ----- start / stop lifecycle ------------------------------------


def test_start_stop_idempotent(tmp_path: Path) -> None:
    daemon = SyncDaemon(InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=60)
    daemon.start()
    daemon.start()  # idempotent — must not spawn a second thread.
    daemon.stop()
    daemon.stop()  # idempotent stop.


def test_background_thread_runs_at_least_one_tick(tmp_path: Path) -> None:
    db = tmp_path / "lib.db"
    lib = TrackLibrary(db)
    lib.add_track(_make_track("t"))
    lib.close()
    client = InMemorySyncClient()
    daemon = SyncDaemon(client, db, tick_seconds=0.05)
    daemon.start()
    # Give the thread enough wall-clock to fire ≥1 tick + write back.
    deadline = time.time() + 2.0
    while time.time() < deadline:
        if client.get_track("t") is not None:
            break
        time.sleep(0.05)
    daemon.stop()
    assert client.get_track("t") is not None


def test_daemon_swallows_tick_exceptions(tmp_path: Path) -> None:
    """A bad client must not crash the daemon — log + continue."""
    db = tmp_path / "lib.db"
    lib = TrackLibrary(db)
    lib.add_track(_make_track("t"))
    lib.close()

    class FlakyThenStableClient:
        def __init__(self) -> None:
            self._calls = 0
            self.calmed_down = False

        def list_tracks(self, *, since_micros: int = 0):
            self._calls += 1
            if self._calls < 2:
                raise RuntimeError("transient")
            self.calmed_down = True
            return []

        def get_track(self, _id):
            return None

        def upsert_track(self, _t):
            return None

        def delete_track(self, _id):
            return False

    client = FlakyThenStableClient()
    daemon = SyncDaemon(client, db, tick_seconds=0.05)
    daemon.start()
    # Wait long enough for ≥2 ticks → second tick should succeed.
    deadline = time.time() + 2.0
    while time.time() < deadline:
        if client.calmed_down:
            break
        time.sleep(0.05)
    daemon.stop()
    assert client.calmed_down, "daemon should survive a thrown tick"


def test_stats_zero_before_first_tick(tmp_path: Path) -> None:
    daemon = SyncDaemon(InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=60)
    s = daemon.stats()
    assert s.last_pull_micros == 0
    assert s.last_push_micros == 0
    assert s.last_pull_fetched == 0
    assert s.last_pull_applied == 0
    assert s.last_push_pushed == 0
    assert s.last_tick_error == ""


def test_stats_populated_after_tick_once(tmp_path: Path) -> None:
    db = tmp_path / "lib.db"
    lib = TrackLibrary(db)
    lib.add_track(_make_track("t1"))
    lib.add_track(_make_track("t2"))
    lib.close()
    client = InMemorySyncClient(
        seed=[
            RemoteTrack(
                track_id="remote-a",
                path="/r/a.mp3",
                bpm=120.0,
                camelot_key="8B",
                energy=0.5,
                duration_s=180.0,
                updated_at_micros=42,
            ),
        ],
    )
    daemon = SyncDaemon(client, db, tick_seconds=60)
    daemon.tick_once()
    s = daemon.stats()
    assert s.last_pull_micros > 0
    assert s.last_push_micros > 0
    assert s.last_pull_fetched == 1  # the cloud row
    assert s.last_pull_applied == 1
    assert s.last_push_pushed == 2  # the two local rows
    assert s.last_tick_error == ""


def test_daemon_swallows_sync_error_specifically(
    tmp_path: Path, caplog
) -> None:
    """Transport SyncError logs at WARN, not ERROR — common case."""
    import logging as _logging

    from copilot.cloud_sync import SyncError as _SyncError

    db = tmp_path / "lib.db"
    lib = TrackLibrary(db)
    lib.add_track(_make_track("t"))
    lib.close()

    class FlakyClient:
        def __init__(self) -> None:
            self._calls = 0
            self.recovered = False

        def list_tracks(self, *, since_micros: int = 0):
            self._calls += 1
            if self._calls < 2:
                raise _SyncError("backend 503")
            self.recovered = True
            return []

        def get_track(self, _id):
            return None

        def upsert_track(self, _t):
            return None

        def delete_track(self, _id):
            return False

    client = FlakyClient()
    daemon = SyncDaemon(client, db, tick_seconds=0.05)
    with caplog.at_level(_logging.WARNING, logger="copilot.cloud_sync.daemon"):
        daemon.start()
        deadline = time.time() + 2.0
        while time.time() < deadline:
            if client.recovered:
                break
            time.sleep(0.05)
        daemon.stop()
    assert client.recovered
    # Transport-error log line must have appeared at WARN, not ERROR.
    warn_msgs = [r for r in caplog.records if r.levelno == _logging.WARNING]
    assert any("transport error" in r.message for r in warn_msgs)
    error_msgs = [r for r in caplog.records if r.levelno >= _logging.ERROR]
    assert not error_msgs, (
        f"SyncError should NOT escalate to ERROR; saw: {error_msgs}"
    )
