"""LibrarySyncer — orchestration on top of :class:`SyncClient`.

Slice 1 ships pull-only:

* On startup the syncer pulls every row whose
  ``updated_at_micros >= since_micros`` (default 0 → full pull).
* Each pulled row is handed to a caller-supplied ``upsert_local``
  callback. The callback returns the local row's stored
  ``updated_at_micros`` so the syncer can run the last-write-wins
  comparison without taking a dependency on the library type.
* Conflict outcome is reported per row so callers can log + surface
  metrics.

Outbound push is stubbed: ``queue_local_change`` exists so future
slices can capture local writes for the eventual push pass.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Callable, Optional

from .client import RemoteTrack, SyncClient, SyncError


class ConflictOutcome(str, Enum):
    """Per-row last-write-wins decision."""

    REMOTE_APPLIED = "remote_applied"
    LOCAL_KEPT = "local_kept"
    LOCAL_INSERTED = "local_inserted"  # row didn't exist locally; remote took it.


@dataclass(frozen=True)
class PullResult:
    """Outcome of a single ``LibrarySyncer.pull`` invocation."""

    fetched_count: int
    applied_count: int  # rows actually written to local store
    kept_local_count: int  # rows skipped because local was newer
    inserted_count: int  # rows that didn't exist locally before the pull
    transport_error: str | None = None  # set when SyncClient raised.
    track_outcomes: dict[str, ConflictOutcome] = field(default_factory=dict)


# Caller plugs in two callbacks:
# 1. `local_updated_at` — read the stored updated_at_micros for a row,
#    or `None` when the row doesn't exist locally yet.
# 2. `apply_remote` — write the remote row into the local store. Free
#    to return early if the syncer already decided to keep local; the
#    callback is only invoked on REMOTE_APPLIED + LOCAL_INSERTED.
LocalUpdatedAt = Callable[[str], Optional[int]]
ApplyRemote = Callable[[RemoteTrack], None]


class LibrarySyncer:
    """Top-level cloud-sync orchestration.

    Wire it up once at copilot startup with:

        client = SupabaseSyncClient.from_env()
        syncer = LibrarySyncer(client, library.local_updated_at_of, library.upsert_from_remote)
        syncer.pull()                # one-shot — slice 1
        # syncer.start_background()  # slice 2 — periodic pull + push.
    """

    def __init__(
        self,
        client: SyncClient,
        local_updated_at: LocalUpdatedAt,
        apply_remote: ApplyRemote,
    ) -> None:
        self._client = client
        self._local_updated_at = local_updated_at
        self._apply_remote = apply_remote

    def pull(self, *, since_micros: int = 0) -> PullResult:
        """Run a single pull pass.

        Never raises — transport errors are captured in
        ``PullResult.transport_error`` so the caller can log + continue.
        Local writes happen synchronously inside ``apply_remote``; if
        that callback raises the syncer logs it via the result + moves
        on (one bad row shouldn't take down the whole pull).
        """
        try:
            rows = self._client.list_tracks(since_micros=since_micros)
        except SyncError as exc:
            return PullResult(
                fetched_count=0,
                applied_count=0,
                kept_local_count=0,
                inserted_count=0,
                transport_error=str(exc),
            )
        applied = 0
        kept = 0
        inserted = 0
        outcomes: dict[str, ConflictOutcome] = {}
        for row in rows:
            local_ts = self._local_updated_at(row.track_id)
            if local_ts is None:
                self._apply_remote(row)
                inserted += 1
                outcomes[row.track_id] = ConflictOutcome.LOCAL_INSERTED
                continue
            if row.updated_at_micros > local_ts:
                self._apply_remote(row)
                applied += 1
                outcomes[row.track_id] = ConflictOutcome.REMOTE_APPLIED
            else:
                kept += 1
                outcomes[row.track_id] = ConflictOutcome.LOCAL_KEPT
        return PullResult(
            fetched_count=len(rows),
            applied_count=applied,
            kept_local_count=kept,
            inserted_count=inserted,
            transport_error=None,
            track_outcomes=outcomes,
        )

    def queue_local_change(self, track: RemoteTrack) -> None:
        """Stub — slice 2 wires the outbound push queue.

        Today this is a no-op so the rest of the codebase can be wired
        in without breakage. Calling it logs nothing; the syncer
        contract just promises the call returns.
        """
        # Intentional no-op (see docstring).
        _ = track
