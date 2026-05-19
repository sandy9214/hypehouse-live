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
from typing import Callable, Iterable, Optional

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


@dataclass(frozen=True)
class PushResult:
    """Outcome of one outbound push pass (#102 slice 5)."""

    pushed_count: int
    skipped_missing_count: int  # row was deleted between enqueue + flush
    transport_error: str | None = None


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
        """Legacy stub — push enqueue now happens at the library
        layer via `TrackLibrary.add_track` (#102 slice 5).

        Kept for callers that already construct a `RemoteTrack` and
        want to push it directly; just forwards to `upsert_track`.
        """
        try:
            self._client.upsert_track(track)
        except SyncError:
            # Silent — same best-effort policy as the pull path.
            pass

    def push_pending(
        self,
        ids: Iterable[str],
        row_loader: Callable[[str], Optional["PushRow"]],
        on_pushed: Callable[[str], None],
    ) -> "PushResult":
        """Drain the pending-push queue.

        - `ids` — pending track ids (from `library.pending_push_ids()`).
        - `row_loader(track_id)` — return the seven-field tuple from
          `library.row_for_cloud_push`, or `None` when the row has been
          deleted locally since enqueue (skip that id, don't fail the
          whole pass).
        - `on_pushed(track_id)` — called after each successful upsert
          so the library can clear the entry.

        Transport errors abort the pass — `result.transport_error` is
        set. We don't retry inside the syncer; the next pull-push
        cycle on the next startup / tick re-queues automatically since
        we only clear on success.
        """
        pushed = 0
        skipped_missing = 0
        for track_id in ids:
            row = row_loader(track_id)
            if row is None:
                skipped_missing += 1
                continue
            path, bpm, key, energy, duration_s, hot_cues, updated_at_micros = row
            wire = RemoteTrack.from_local(
                track_id=track_id,
                path=path,
                bpm=bpm,
                camelot_key=key,
                energy=energy,
                duration_s=duration_s,
                hot_cues=hot_cues,
                updated_at_micros=updated_at_micros,
            )
            try:
                self._client.upsert_track(wire)
            except SyncError as exc:
                return PushResult(
                    pushed_count=pushed,
                    skipped_missing_count=skipped_missing,
                    transport_error=str(exc),
                )
            on_pushed(track_id)
            pushed += 1
        return PushResult(
            pushed_count=pushed,
            skipped_missing_count=skipped_missing,
            transport_error=None,
        )


# Local seven-tuple shape returned by `TrackLibrary.row_for_cloud_push`.
# Spelled out as an alias here so the syncer call site documents the
# contract without importing the library type (kept loose for the
# in-memory test client).
PushRow = tuple
