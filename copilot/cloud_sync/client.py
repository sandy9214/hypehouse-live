"""SyncClient protocol + in-memory fake for tests.

The real Supabase REST adapter (``SupabaseSyncClient``) lands in
slice 2 — same surface, just swaps the dict-backed storage for HTTPS
calls to ``$SUPABASE_URL/rest/v1/tracks``.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Iterable, Protocol


class SyncError(Exception):
    """Transport-layer failure surface for cloud-sync calls.

    Distinguishes wire-level / auth-level failures from domain-level
    "remote row missing" cases (which return ``None`` instead). The
    syncer logs+retries on this; the local library mirror is never
    blocked on cloud-sync errors.
    """


@dataclass(frozen=True)
class RemoteTrack:
    """One library row as it lives in the cloud.

    Matches the Supabase ``tracks`` row schema. ``hot_cues_ms`` is an
    8-element list where unset slots are ``-1`` (sentinel — Supabase
    JSON can't store sparse arrays with explicit nulls inside an
    ``int[]`` column natively).
    """

    track_id: str
    path: str
    bpm: float
    camelot_key: str
    energy: float
    duration_s: float
    hot_cues_ms: tuple[int, ...] = field(default=(-1, -1, -1, -1, -1, -1, -1, -1))
    updated_at_micros: int = 0

    def hot_cues_as_options(self) -> list[int | None]:
        """Decode the ``-1`` sentinel back into ``None`` for local callers."""
        return [None if v < 0 else int(v) for v in self.hot_cues_ms]

    @classmethod
    def from_local(
        cls,
        *,
        track_id: str,
        path: str,
        bpm: float,
        camelot_key: str,
        energy: float,
        duration_s: float,
        hot_cues: list[int | None] | None,
        updated_at_micros: int,
    ) -> "RemoteTrack":
        """Encode a local row into the wire shape — ``None`` → ``-1``.

        Hot-cue list is padded/truncated to 8 entries so the remote
        column shape stays stable regardless of slot count changes.
        """
        cues = list(hot_cues or [])
        cues = (cues + [None] * 8)[:8]
        encoded = tuple((-1 if v is None else int(v)) for v in cues)
        return cls(
            track_id=track_id,
            path=path,
            bpm=bpm,
            camelot_key=camelot_key,
            energy=energy,
            duration_s=duration_s,
            hot_cues_ms=encoded,
            updated_at_micros=updated_at_micros,
        )


class SyncClient(Protocol):
    """Cloud-storage adapter surface.

    Methods are all blocking — the syncer runs on a dedicated thread.
    The Supabase REST adapter implements this with synchronous
    ``requests.Session`` calls (gevent-friendly; aiohttp parity in a
    later slice if the runtime needs it).
    """

    def list_tracks(self, *, since_micros: int = 0) -> list[RemoteTrack]:
        """Pull rows whose ``updated_at_micros >= since_micros``.

        Empty / non-existent table → empty list. Transport errors →
        :class:`SyncError`. Caller orders + dedups; this surface does
        not promise any specific ordering.
        """
        ...

    def get_track(self, track_id: str) -> RemoteTrack | None:
        """Read a single row. ``None`` when the row doesn't exist."""
        ...

    def upsert_track(self, track: RemoteTrack) -> None:
        """Insert-or-update a row. Conflict resolution is last-write-wins
        based on ``updated_at_micros``; the implementation enforces
        that with a Postgres ``ON CONFLICT (track_id) DO UPDATE WHERE
        excluded.updated_at_micros > tracks.updated_at_micros`` clause.
        """
        ...

    def delete_track(self, track_id: str) -> bool:
        """Remove a row. Returns ``True`` if a row was deleted."""
        ...


class InMemorySyncClient:
    """Dict-backed :class:`SyncClient` implementation.

    Used by every test in this module so the suite stays hermetic —
    no live Supabase project needed for CI. Also handy for local-dev
    runs where the operator hasn't yet provisioned credentials.

    Thread-safe: all reads and writes hold a single internal lock.
    The syncer thread + the local library writer thread can call
    concurrently without races. Realistic-enough latency simulation
    is NOT provided here; tests that want to exercise jitter wrap
    this in a sleep-injecting adapter.
    """

    def __init__(
        self, seed: Iterable[RemoteTrack] | None = None
    ) -> None:
        import threading

        self._lock = threading.Lock()
        self._rows: dict[str, RemoteTrack] = {}
        if seed is not None:
            for row in seed:
                self._rows[row.track_id] = row

    def list_tracks(self, *, since_micros: int = 0) -> list[RemoteTrack]:
        with self._lock:
            return [
                row
                for row in self._rows.values()
                if row.updated_at_micros >= since_micros
            ]

    def get_track(self, track_id: str) -> RemoteTrack | None:
        with self._lock:
            return self._rows.get(track_id)

    def upsert_track(self, track: RemoteTrack) -> None:
        with self._lock:
            existing = self._rows.get(track.track_id)
            if existing is None or track.updated_at_micros >= existing.updated_at_micros:
                self._rows[track.track_id] = track

    def delete_track(self, track_id: str) -> bool:
        with self._lock:
            return self._rows.pop(track_id, None) is not None

    def __len__(self) -> int:
        with self._lock:
            return len(self._rows)
