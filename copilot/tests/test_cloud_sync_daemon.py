"""Sync daemon tests (#102 slice 6)."""

from __future__ import annotations

import time
from pathlib import Path

import pytest

from copilot.cloud_sync import (
    InMemorySyncClient,
    RemoteTrack,
    SyncDaemon,
    SyncStats,
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


# ----- exponential backoff ---------------------------------------


def test_next_wait_returns_base_tick_without_failures(tmp_path: Path) -> None:
    daemon = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=60.0
    )
    assert daemon.next_wait_seconds() == 60.0


def test_next_wait_doubles_each_consecutive_failure(tmp_path: Path) -> None:
    daemon = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=30.0
    )
    daemon._consecutive_failures = 1  # noqa: SLF001
    assert daemon.next_wait_seconds() == 60.0
    daemon._consecutive_failures = 2  # noqa: SLF001
    assert daemon.next_wait_seconds() == 120.0
    daemon._consecutive_failures = 3  # noqa: SLF001
    assert daemon.next_wait_seconds() == 240.0


def test_next_wait_caps_at_max_backoff(tmp_path: Path) -> None:
    from copilot.cloud_sync.daemon import MAX_BACKOFF_SECONDS

    daemon = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=60.0
    )
    daemon._consecutive_failures = 5  # noqa: SLF001
    # 60 * 2**5 = 1920s > 600s cap → clamped to 600s
    assert daemon.next_wait_seconds() == MAX_BACKOFF_SECONDS
    daemon._consecutive_failures = 100  # noqa: SLF001
    # Far-out failure counts must still be clamped, not overflow.
    assert daemon.next_wait_seconds() == MAX_BACKOFF_SECONDS


def test_tick_once_resets_consecutive_failures_on_success(
    tmp_path: Path,
) -> None:
    daemon = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=60.0
    )
    daemon._consecutive_failures = 3  # noqa: SLF001
    daemon.tick_once()
    assert daemon._consecutive_failures == 0  # noqa: SLF001


def test_tick_once_preserves_next_sync_micros(tmp_path: Path) -> None:
    """`tick_once` must not touch `next_sync_micros` — only `_loop`
    owns scheduling. An out-of-band `library.sync_now` would
    otherwise advertise a deadline that conflicts with the daemon
    thread's still-pending `_stop.wait(...)`. (Codex #174 R1.)
    """
    daemon = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=60.0
    )
    sentinel = 9_876_543_210_000_000
    # Stamp a fake deadline as if `_loop` had set it.
    daemon._stamp_next_sync(60.0)  # noqa: SLF001
    with daemon._stats_lock:  # noqa: SLF001
        s0 = daemon._stats  # noqa: SLF001
        daemon._stats = SyncStats(  # noqa: SLF001
            last_pull_micros=s0.last_pull_micros,
            last_push_micros=s0.last_push_micros,
            last_pull_fetched=s0.last_pull_fetched,
            last_pull_applied=s0.last_pull_applied,
            last_push_pushed=s0.last_push_pushed,
            last_tick_error=s0.last_tick_error,
            next_sync_micros=sentinel,
        )
    daemon.tick_once()
    assert daemon.stats().next_sync_micros == sentinel


def test_loop_stamps_next_sync_before_wait(tmp_path: Path) -> None:
    """`_loop` stamps `next_sync_micros` right before `_stop.wait` so
    the field reflects the actual upcoming wake."""
    db = tmp_path / "lib.db"
    daemon = SyncDaemon(
        InMemorySyncClient(), db, tick_seconds=60.0
    )
    before = int(time.time() * 1_000_000)
    daemon._stamp_next_sync(daemon.next_wait_seconds())  # noqa: SLF001
    after = int(time.time() * 1_000_000)
    s = daemon.stats()
    # Clean state → schedule = now + 60s. Wide slop tolerates clock
    # readings on either side of the stamp.
    expected_min = before + 59_900_000
    expected_max = after + 60_100_000
    assert expected_min <= s.next_sync_micros <= expected_max


def test_sync_now_does_not_clobber_pending_wake_deadline(
    tmp_path: Path,
) -> None:
    """Regression: Codex's #174 R1 finding.

    Scenario: daemon was in a long backoff wait (e.g. 5 prior
    failures → 1920s capped to 600s). Operator clicks "Sync now"
    via `library.sync_now`, which runs `tick_once` out-of-band.
    `tick_once` must NOT advertise a fresh "next in 60s" deadline
    because the daemon thread is still blocked on the old wait;
    only `_loop` (re-stamping before its next `_stop.wait`) gets to
    move the deadline.
    """
    daemon = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=60.0
    )
    # Pretend the loop just stamped a 10-minute deadline based on 5
    # prior failures (capped at MAX_BACKOFF_SECONDS).
    daemon._consecutive_failures = 5  # noqa: SLF001
    daemon._stamp_next_sync(daemon.next_wait_seconds())  # noqa: SLF001
    deadline_before = daemon.stats().next_sync_micros

    # Operator clicks "Sync now" — runs an out-of-band tick that
    # succeeds. This resets _consecutive_failures (good) but MUST
    # leave next_sync_micros alone (loop owns it).
    daemon.tick_once()
    deadline_after = daemon.stats().next_sync_micros
    assert deadline_after == deadline_before, (
        "tick_once must not move next_sync_micros — only _loop does"
    )
    # _consecutive_failures still got reset by the clean tick — the
    # next `_loop` iteration will re-stamp at 60s when it wakes.
    assert daemon._consecutive_failures == 0  # noqa: SLF001


def test_stamp_next_sync_grows_under_failures(tmp_path: Path) -> None:
    daemon = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=60.0
    )
    daemon._consecutive_failures = 2  # noqa: SLF001
    daemon._stamp_next_sync(daemon.next_wait_seconds())  # noqa: SLF001
    s = daemon.stats()
    delta = s.next_sync_micros - int(time.time() * 1_000_000)
    # 2 failures → 60 * 4 = 240s.
    assert 239_000_000 <= delta <= 241_000_000


# ----- wake_now -------------------------------------------------


def test_wake_now_short_circuits_long_backoff_wait(tmp_path: Path) -> None:
    """`wake_now` must unblock the daemon thread early so the next
    automatic tick fires soon after a manual `sync_now`. Without
    this, the daemon would finish its long backoff wait before its
    next iteration despite `_consecutive_failures` having been
    reset.
    """
    db = tmp_path / "lib.db"
    lib = TrackLibrary(db)
    lib.add_track(_make_track("t"))
    lib.close()
    client = InMemorySyncClient()

    # Set a tick that's long enough that the test would hang on it
    # without wake_now (and the test's 2s budget would still report
    # the bug as a failure rather than a hang).
    daemon = SyncDaemon(client, db, tick_seconds=5.0)
    daemon.start()
    try:
        # Wait until the thread has parked in `_wake.wait(...)`; the
        # initial iteration stamps `next_sync_micros` so we can use
        # that as a readiness probe.
        deadline = time.time() + 1.0
        while time.time() < deadline:
            if daemon.stats().next_sync_micros > 0:
                break
            time.sleep(0.02)
        assert daemon.stats().next_sync_micros > 0, (
            "daemon never stamped its first wake deadline"
        )

        start = time.time()
        # `skip_next_tick=False` keeps the original test contract —
        # we want this test to assert the wake-from-backoff
        # behavior, not the skip-redundant-tick path (which has its
        # own dedicated test below).
        daemon.wake_now(skip_next_tick=False)
        # Daemon should now wake, run the tick, and push the seeded
        # track to the InMemory client. Without wake_now, this would
        # take ~5s; with it, sub-second.
        budget_deadline = time.time() + 2.0
        while time.time() < budget_deadline:
            if client.get_track("t") is not None:
                break
            time.sleep(0.02)
        assert client.get_track("t") is not None, (
            "daemon never woke + ticked within 2s of wake_now"
        )
        elapsed = time.time() - start
        # Should be well under the 5s tick_seconds (allow generous
        # 1.5s for thread + RPC latency on slow CI runners).
        assert elapsed < 1.5, (
            f"daemon woke too slowly: {elapsed:.2f}s — wake_now ineffective"
        )
    finally:
        daemon.stop()


def test_wake_now_default_skips_redundant_auto_tick(
    tmp_path: Path,
) -> None:
    """`library.sync_now` runs an out-of-band `tick_once` then calls
    `wake_now()`. The daemon's very next iteration would duplicate
    the pull+push (Codex review note on #176). With the default
    `skip_next_tick=True`, the daemon must instead consume the flag
    and skip exactly one iteration's work, then run the iteration
    after that normally.

    The test uses a long `tick_seconds=60.0` so iterations don't
    fire on their own — only the explicit `wake_now` calls drive
    the daemon. That keeps the assertion deterministic.
    """

    class CountingClient:
        def __init__(self) -> None:
            self.list_tracks_calls = 0

        def list_tracks(self, *, since_micros=0):  # noqa: ARG002
            self.list_tracks_calls += 1
            return []

        def get_track(self, _id):
            return None

        def upsert_track(self, _t):
            return None

        def delete_track(self, _id):
            return False

    client = CountingClient()
    daemon = SyncDaemon(client, tmp_path / "lib.db", tick_seconds=60.0)
    daemon.start()
    try:
        # Settle: wait for the daemon to enter its first
        # _wake.wait(60s).
        deadline = time.time() + 1.0
        while time.time() < deadline:
            if daemon.stats().next_sync_micros > 0:
                break
            time.sleep(0.01)
        baseline = client.list_tracks_calls  # 0

        # First wake: force a real tick (no skip). Counter should
        # increment by 1.
        daemon.wake_now(skip_next_tick=False)
        # Give the daemon a moment to run the tick and re-enter
        # _wake.wait.
        for _ in range(50):
            if client.list_tracks_calls > baseline:
                break
            time.sleep(0.02)
        after_first = client.list_tracks_calls
        assert after_first == baseline + 1

        # Second wake: default skip_next_tick=True. The daemon
        # should wake, see the flag, and skip the tick. Counter
        # must NOT increment.
        daemon.wake_now()
        # Wait a window that's plenty long for a tick if one were
        # to fire (it shouldn't), but well under the 60s cadence.
        time.sleep(0.3)
        after_skip = client.list_tracks_calls
        assert after_skip == after_first, (
            f"skip flag failed: counter went {after_first} -> {after_skip}"
        )
    finally:
        daemon.stop()


def test_stop_unblocks_wake_wait(tmp_path: Path) -> None:
    """`stop()` must signal `_wake` too — otherwise the daemon thread
    sits in `_wake.wait(tick)` until the timeout fires even after
    the operator asks it to stop."""
    daemon = SyncDaemon(
        InMemorySyncClient(), tmp_path / "lib.db", tick_seconds=10.0
    )
    daemon.start()
    # Wait for the daemon to enter its first wait.
    deadline = time.time() + 1.0
    while time.time() < deadline:
        if daemon.stats().next_sync_micros > 0:
            break
        time.sleep(0.02)
    start = time.time()
    daemon.stop(join_timeout_s=2.0)
    elapsed = time.time() - start
    # Must unblock fast — well under the 10s wait length.
    assert elapsed < 1.5, (
        f"stop() didn't break the wake-wait early: {elapsed:.2f}s"
    )


def test_consecutive_failures_mutation_is_lock_protected(
    tmp_path: Path,
) -> None:
    """Two threads incrementing _consecutive_failures via tick_once
    must never lose an increment to a torn read-modify-write.
    `sync_now` (via the RPC handler thread) + the daemon `_loop`
    (its own thread) exercise this in production.
    """
    import threading

    class FlakyPullClient:
        """Same shape as the test below — every tick records a
        transport_error so the counter bumps."""

        def list_tracks(self, *, since_micros=0):  # noqa: ARG002
            from copilot.cloud_sync.client import SyncError

            raise SyncError("HTTP 503")

        def get_track(self, _id):
            return None

        def upsert_track(self, _t):
            return None

        def delete_track(self, _id):
            return False

    daemon = SyncDaemon(
        FlakyPullClient(), tmp_path / "lib.db", tick_seconds=60.0
    )
    N_PER_THREAD = 50
    barrier = threading.Barrier(2)

    def worker() -> None:
        barrier.wait()
        for _ in range(N_PER_THREAD):
            daemon.tick_once()

    t1 = threading.Thread(target=worker)
    t2 = threading.Thread(target=worker)
    t1.start()
    t2.start()
    t1.join(timeout=10.0)
    t2.join(timeout=10.0)
    # 100 ticks all errored → counter must be exactly 100. Without
    # locking the read-modify-write, lost updates would land here as
    # a value < 100.
    assert daemon._consecutive_failures == 2 * N_PER_THREAD  # noqa: SLF001


def test_tick_once_bumps_failures_on_transport_error(
    tmp_path: Path,
) -> None:
    """When pull or push records a `transport_error`, the daemon
    records the failure so the next sleep backs off."""

    class FlakyPullClient:
        """Returns a transport-error via the public list_tracks
        exception path that bootstrap_pull catches → PullResult
        with transport_error set."""

        def list_tracks(self, *, since_micros=0):  # noqa: ARG002
            from copilot.cloud_sync.client import SyncError

            raise SyncError("HTTP 503")

        def get_track(self, _id):
            return None

        def upsert_track(self, _t):
            return None

        def delete_track(self, _id):
            return False

    daemon = SyncDaemon(
        FlakyPullClient(), tmp_path / "lib.db", tick_seconds=60.0
    )
    daemon.tick_once()
    assert daemon._consecutive_failures == 1  # noqa: SLF001
    daemon.tick_once()
    assert daemon._consecutive_failures == 2  # noqa: SLF001
    # Backoff schedule reflects the count.
    assert daemon.next_wait_seconds() == 240.0  # 60 * 4
