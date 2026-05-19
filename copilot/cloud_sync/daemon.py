"""Periodic background cloud sync (#102 slice 6 — final).

Runs `bootstrap_pull` + `bootstrap_push` on a fixed cadence. Single
thread; thread-safe shutdown via an `Event`. The library + sync
client are both required to handle concurrent access from a separate
thread — `TrackLibrary` does (SQLite check_same_thread=False isn't
enabled, but the bootstrap helpers use the same connection through
the daemon's loop only, and the library is opened on the main
thread); `InMemorySyncClient` is internally locked.

For SQLite specifically, the daemon needs its own short-lived
`TrackLibrary` instance — sqlite3 connections aren't safe to share
across threads by default. Wire-up at startup either passes the
library DB path (preferred — daemon opens its own connection per
tick) or accepts a "library access" callback that the caller serializes.

This module ships the path-based variant: each tick opens a
`TrackLibrary` against the same DB file, runs pull + push, closes.
SQLite's process-level locking serializes the writes against the
main thread.
"""

from __future__ import annotations

import logging
import os
import sqlite3
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from .bootstrap import bootstrap_pull, bootstrap_push
from .client import SyncClient, SyncError


@dataclass(frozen=True)
class SyncStats:
    """Snapshot of the daemon's last-tick counters.

    Wall-clock micros (same scale as `Event.ts_micros`). All fields
    `0` before the first tick — UI renders "never synced" then.
    """

    last_pull_micros: int = 0
    last_push_micros: int = 0
    last_pull_fetched: int = 0
    last_pull_applied: int = 0
    last_push_pushed: int = 0
    last_tick_error: str = ""

DEFAULT_TICK_SECONDS = 60.0


class SyncDaemon:
    """Background pull-push loop. Stop with `.stop()`."""

    def __init__(
        self,
        client: SyncClient,
        library_path: str | Path,
        *,
        tick_seconds: float = DEFAULT_TICK_SECONDS,
        logger: Optional[logging.Logger] = None,
    ) -> None:
        self._client = client
        self._library_path = Path(library_path)
        self._tick = max(0.01, float(tick_seconds))
        self._log = logger or logging.getLogger("copilot.cloud_sync.daemon")
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._stats_lock = threading.Lock()
        self._stats = SyncStats()

    @classmethod
    def from_env(
        cls,
        client: SyncClient,
        library_path: str | Path,
        *,
        env_var: str = "HYPEHOUSE_SYNC_TICK_SECONDS",
        logger: Optional[logging.Logger] = None,
    ) -> "SyncDaemon":
        """Construct with `tick_seconds` from env (default 60s).

        Non-positive / non-numeric values silently fall back to the
        default so a typo in the launchd plist doesn't disable the
        daemon outright.
        """
        raw = os.environ.get(env_var, "").strip()
        tick = DEFAULT_TICK_SECONDS
        if raw:
            try:
                parsed = float(raw)
                if parsed > 0:
                    tick = parsed
            except ValueError:
                pass
        return cls(
            client,
            library_path,
            tick_seconds=tick,
            logger=logger,
        )

    def start(self) -> None:
        """Spawn the daemon thread. Idempotent — second call is a no-op."""
        if self._thread is not None and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(
            target=self._loop,
            name="copilot-cloud-sync",
            daemon=True,
        )
        self._thread.start()
        self._log.info(
            "cloud sync: daemon started (tick=%.1fs, db=%s)",
            self._tick,
            self._library_path,
        )

    def stop(self, *, join_timeout_s: float = 5.0) -> None:
        """Signal stop and join. Idempotent."""
        self._stop.set()
        thread = self._thread
        if thread is not None and thread.is_alive():
            thread.join(timeout=join_timeout_s)
        self._thread = None

    def tick_once(self) -> None:
        """Run a single pull + push cycle (test seam).

        Records counters into `self._stats` so the UI badge can show
        "last synced X ago" without polling the cloud directly. The
        record happens under `_stats_lock` so an RPC reader thread
        sees a consistent snapshot.
        """
        # Lazy-import to avoid pulling library.py into this module's
        # cold-start path when the daemon isn't used (e.g. local-only
        # mode where the sync client is the InMemory fallback).
        from copilot.library import TrackLibrary

        library = TrackLibrary(self._library_path)
        try:
            pull_result = bootstrap_pull(
                self._client, library=library, logger=self._log
            )
            push_result = bootstrap_push(
                self._client, library, logger=self._log
            )
            now_us = int(time.time() * 1_000_000)
            with self._stats_lock:
                self._stats = SyncStats(
                    last_pull_micros=now_us,
                    last_push_micros=now_us,
                    last_pull_fetched=pull_result.fetched_count,
                    last_pull_applied=pull_result.applied_count
                    + pull_result.inserted_count,
                    last_push_pushed=push_result.pushed_count,
                    last_tick_error=(
                        pull_result.transport_error
                        or push_result.transport_error
                        or ""
                    ),
                )
        finally:
            library.close()

    def stats(self) -> SyncStats:
        """Thread-safe snapshot of the last-tick counters. Returns a
        fresh frozen dataclass so the caller can read without holding
        the lock.
        """
        with self._stats_lock:
            return self._stats

    def _loop(self) -> None:
        # Wait one tick BEFORE the first sync so we don't double up
        # with the bootstrap pull/push that already ran at startup.
        while not self._stop.wait(self._tick):
            try:
                self.tick_once()
            except SyncError as exc:
                # Transport-level failures are expected — flaky cloud
                # is the whole reason this is async + retried.
                self._log.warning(
                    "cloud sync: transport error during tick: %s", exc
                )
            except sqlite3.Error as exc:
                # Local-DB hiccup (lock contention with the main
                # thread's writes, busy timeout, etc.). Recoverable —
                # next tick re-opens the connection.
                self._log.warning(
                    "cloud sync: local DB error during tick: %s", exc
                )
            except Exception as exc:  # noqa: BLE001
                # Anything else is unexpected — log at ERROR so an
                # operator tailing the log sees it. We still don't
                # re-raise; killing the daemon thread would silently
                # disable cloud sync for the rest of the process.
                self._log.error(
                    "cloud sync: unexpected daemon tick error: %s",
                    exc,
                    exc_info=exc,
                )
            # Light sleep avoids a tight error-loop on a misbehaving
            # client; the `Event.wait` above is the main pacing wait.
            time.sleep(0)


__all__ = ["DEFAULT_TICK_SECONDS", "SyncDaemon", "SyncStats"]
