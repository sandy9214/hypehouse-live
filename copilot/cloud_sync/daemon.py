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
import threading
import time
from pathlib import Path
from typing import Optional

from .bootstrap import bootstrap_pull, bootstrap_push
from .client import SyncClient

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
        """Run a single pull + push cycle (test seam)."""
        # Lazy-import to avoid pulling library.py into this module's
        # cold-start path when the daemon isn't used (e.g. local-only
        # mode where the sync client is the InMemory fallback).
        from copilot.library import TrackLibrary

        library = TrackLibrary(self._library_path)
        try:
            bootstrap_pull(self._client, library=library, logger=self._log)
            bootstrap_push(self._client, library, logger=self._log)
        finally:
            library.close()

    def _loop(self) -> None:
        # Wait one tick BEFORE the first sync so we don't double up
        # with the bootstrap pull/push that already ran at startup.
        while not self._stop.wait(self._tick):
            try:
                self.tick_once()
            except Exception as exc:  # noqa: BLE001
                # Swallow + log — a bad tick should never crash the
                # process. Cloud-sync errors are not user-fatal; they
                # only delay propagation.
                self._log.warning(
                    "cloud sync: daemon tick raised %s — continuing", exc
                )
            # Light sleep avoids a tight error-loop on a misbehaving
            # client; the `Event.wait` above is the main pacing wait.
            time.sleep(0)


__all__ = ["DEFAULT_TICK_SECONDS", "SyncDaemon"]
