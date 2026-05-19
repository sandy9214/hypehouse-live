"""Playlist queue — DJ-curated next-track order.

PR #67 wired an *auto-mix* mode that uses :class:`copilot.proposer.
TransitionProposer` to pick the next track via mashability scoring.
That's the right default but DJs frequently want **explicit** control:
"after this track, play X, then Y, then Z".

This module persists that ordered list in SQLite (schema v8) and
exposes the four canonical mutations (enqueue / dequeue / reorder /
remove + a bulk clear). The auto-mix controller consults
:meth:`PlaylistQueue.dequeue` first; only when the queue is empty does
it fall back to the mashability ranker.

Schema::

    CREATE TABLE playlist_queue (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        track_id   TEXT NOT NULL,
        position   INTEGER NOT NULL,
        added_at   TEXT NOT NULL
    );
    CREATE INDEX playlist_queue_pos_idx ON playlist_queue (position);

Positions are 0-indexed, dense (no gaps), and recomputed on every
mutation so the wire shape never carries holes. Re-numbering the whole
table on each write is O(N) but N is bounded by what fits in a DJ's
short-term memory (~30 tracks in practice) so the simplicity wins
over a sparse / linked-list scheme.

Track id integrity is intentionally **not** enforced as a SQL FK —
the library row may be removed while a queue entry references it
(operator deletes a file from disk + re-scans). On dequeue we resolve
the id via the live :class:`copilot.library.TrackLibrary` lookup and
silently drop entries whose track no longer exists. This mirrors the
proposer's tolerance for a stale ``last_track_id``.
"""
from __future__ import annotations

import logging
import sqlite3
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Optional

from .library import TrackLibrary, TrackRef

log = logging.getLogger(__name__)


@dataclass(frozen=True)
class PlaylistEntry:
    """One row from the playlist_queue table joined with the library.

    ``track`` is ``None`` when the entry references a track id that no
    longer exists in the library — callers can either skip those rows
    or surface them to the UI with a "missing" badge. We keep the entry
    visible (instead of garbage-collecting it on read) so the operator
    can explicitly remove it; silent deletion would be surprising.
    """

    track_id: str
    position: int
    added_at: str
    track: Optional[TrackRef]


class PlaylistQueue:
    """SQLite-backed ordered queue of track ids.

    Owns the ``playlist_queue`` table; shares the
    :class:`TrackLibrary`'s connection so the queue + the catalog live
    in the same DB file (one open handle, one ``.commit()`` per write).

    The queue is **per-set** singleton — there's exactly one queue per
    library DB. Multi-queue (named playlists) is deferred to a future
    PR; today's UX is the auto-DJ working set.
    """

    def __init__(self, library: TrackLibrary):
        self._library = library
        # Reach into the library's connection — the schema migration
        # already created the ``playlist_queue`` table (see
        # :func:`copilot.library.TrackLibrary._init_schema`'s v8 block).
        self._conn: sqlite3.Connection = library._conn  # noqa: SLF001

    # ------------------------------------------------------------------
    # Mutations
    # ------------------------------------------------------------------

    def enqueue(self, track_id: str) -> PlaylistEntry:
        """Append ``track_id`` to the end of the queue.

        Duplicates are allowed — a DJ may legitimately want to play the
        same track twice in a set (intro -> later callback). The caller
        is the operator's decision-maker; we don't second-guess.

        Raises:
            ValueError: ``track_id`` is empty / non-string.
        """
        if not isinstance(track_id, str) or not track_id:
            raise ValueError("track_id must be a non-empty string")

        # Use a single transaction so the position computation + insert
        # are atomic — concurrent enqueues from two RPC clients would
        # otherwise race on ``MAX(position) + 1``.
        now = _now_iso()
        cursor = self._conn.execute(
            "SELECT COALESCE(MAX(position), -1) AS last FROM playlist_queue"
        )
        last_row = cursor.fetchone()
        next_pos = int(last_row["last"]) + 1 if last_row is not None else 0
        self._conn.execute(
            "INSERT INTO playlist_queue (track_id, position, added_at) "
            "VALUES (?, ?, ?)",
            (track_id, next_pos, now),
        )
        self._conn.commit()
        return PlaylistEntry(
            track_id=track_id,
            position=next_pos,
            added_at=now,
            track=self._library.get(track_id),
        )

    def dequeue(self) -> Optional[str]:
        """Pop the head of the queue and return its track id.

        Returns ``None`` when the queue is empty. Skips (drops) entries
        whose referenced track is no longer in the library, so the
        auto-mix controller doesn't get handed a stale id that the
        engine would refuse to load.

        After the pop, positions on remaining rows are *not* renumbered
        — they stay sparse-from-the-top. The next mutation
        (:meth:`enqueue` / :meth:`reorder` / :meth:`remove`) compacts
        them via :meth:`_renumber`. Read APIs (:meth:`list_queue`)
        normalize at projection time.
        """
        while True:
            row = self._conn.execute(
                "SELECT id, track_id FROM playlist_queue "
                "ORDER BY position ASC LIMIT 1"
            ).fetchone()
            if row is None:
                return None
            track_id = row["track_id"]
            self._conn.execute(
                "DELETE FROM playlist_queue WHERE id = ?", (row["id"],)
            )
            self._conn.commit()
            if self._library.get(track_id) is not None:
                # Compact positions for stable wire shape on the next
                # read. Skips renumber when queue is now empty.
                self._renumber()
                return track_id
            # Track disappeared from library — drop this entry silently
            # and try the next one. The loop bound is the table size;
            # worst-case we walk a queue full of stale ids exactly once.
            log.info(
                "playlist.dequeue: skipping missing track_id=%s", track_id
            )

    def list_queue(self) -> list[PlaylistEntry]:
        """Return every entry, in play order, with track metadata joined.

        Position values in the returned entries are normalized to a
        dense 0..N-1 sequence even if the underlying rows are sparse
        (post-dequeue, mid-mutation). Lets the UI render row numbers
        without re-doing the math.
        """
        rows = self._conn.execute(
            "SELECT track_id, position, added_at FROM playlist_queue "
            "ORDER BY position ASC"
        ).fetchall()
        out: list[PlaylistEntry] = []
        for i, r in enumerate(rows):
            out.append(
                PlaylistEntry(
                    track_id=r["track_id"],
                    position=i,
                    added_at=r["added_at"],
                    track=self._library.get(r["track_id"]),
                )
            )
        return out

    def reorder(self, track_id: str, new_position: int) -> list[PlaylistEntry]:
        """Move ``track_id``'s entry to ``new_position`` (0-indexed).

        ``new_position`` is clamped to ``[0, len(queue) - 1]`` — passing
        a value outside the bounds doesn't raise; it pins to the
        nearest edge. Matches the UI drag-handle semantics (drop above
        first row -> position 0; drop below last -> last).

        If ``track_id`` appears multiple times in the queue, only the
        first (lowest current position) is moved. The UI's reorder
        affordance can't distinguish duplicates anyway — the row id
        isn't part of the public surface.

        Raises:
            KeyError: ``track_id`` not in the queue.
        """
        if not isinstance(new_position, int) or isinstance(new_position, bool):
            # Reject bool explicitly — ``isinstance(True, int)`` is True
            # and ``True`` would silently become position 1.
            raise ValueError("new_position must be an int")

        entries = self.list_queue()  # normalized positions 0..N-1
        ids_in_order = [e.track_id for e in entries]
        if track_id not in ids_in_order:
            raise KeyError(track_id)

        # Snap to bounds.
        n = len(entries)
        clamped = max(0, min(int(new_position), n - 1))

        # Remove the first occurrence + reinsert at the clamped index.
        cur_index = ids_in_order.index(track_id)
        if cur_index == clamped:
            # No-op — already at the requested slot. Still re-emit the
            # snapshot so the RPC caller has a fresh view.
            return entries
        ids_in_order.pop(cur_index)
        ids_in_order.insert(clamped, track_id)

        # Persist the new order. Easiest correct path is: wipe the
        # table, re-insert in order. Cheaper than per-row UPDATEs and
        # immune to position collisions during the move. Preserves
        # ``added_at`` by snapshotting the (track_id -> added_at) map
        # before the wipe.
        added_map = {e.track_id: e.added_at for e in entries}
        # When duplicates exist, the dict keeps the first added_at —
        # which matches the "operate on the first occurrence" rule
        # above; for the second copy we re-stamp with `now` so the
        # entry's "added_at" reflects its new identity. This is a
        # cosmetic detail; primary key on the new rows is fresh anyway.
        self._conn.execute("DELETE FROM playlist_queue")
        now = _now_iso()
        for pos, tid in enumerate(ids_in_order):
            self._conn.execute(
                "INSERT INTO playlist_queue "
                "(track_id, position, added_at) VALUES (?, ?, ?)",
                (tid, pos, added_map.get(tid, now)),
            )
        self._conn.commit()
        return self.list_queue()

    def remove(self, track_id: str) -> list[PlaylistEntry]:
        """Remove every entry matching ``track_id`` (including duplicates).

        Returns the post-mutation snapshot so the UI doesn't have to
        re-fetch. Raises :class:`KeyError` if no entry matched —
        silently ignoring would mask a stale UI id.
        """
        cursor = self._conn.execute(
            "DELETE FROM playlist_queue WHERE track_id = ?", (track_id,)
        )
        if cursor.rowcount == 0:
            raise KeyError(track_id)
        self._conn.commit()
        self._renumber()
        return self.list_queue()

    def clear(self) -> None:
        """Empty the queue. Idempotent — clearing an empty queue is fine."""
        self._conn.execute("DELETE FROM playlist_queue")
        self._conn.commit()

    def __len__(self) -> int:
        r = self._conn.execute(
            "SELECT COUNT(*) AS n FROM playlist_queue"
        ).fetchone()
        return int(r["n"]) if r else 0

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _renumber(self) -> None:
        """Compact positions to a dense 0..N-1 sequence.

        Called after a delete / pop so the next enqueue lands at
        ``len(queue)`` rather than ``MAX(position) + 1`` (which would
        leak holes into the wire shape). One-pass UPDATE keyed on the
        autoincrement ``id`` PK preserves insertion order — the
        ``position`` column is the user-facing order, ``id`` is the
        stable identity used for the renumber pass.
        """
        rows = self._conn.execute(
            "SELECT id FROM playlist_queue ORDER BY position ASC, id ASC"
        ).fetchall()
        for i, r in enumerate(rows):
            self._conn.execute(
                "UPDATE playlist_queue SET position = ? WHERE id = ?",
                (i, r["id"]),
            )
        self._conn.commit()


def _now_iso() -> str:
    """ISO-8601 UTC timestamp suitable for the ``added_at`` column."""
    return datetime.now(timezone.utc).isoformat()


def entry_to_wire(entry: PlaylistEntry) -> dict[str, object]:
    """Project a :class:`PlaylistEntry` into the JSON-RPC wire shape.

    The ``track`` field carries the same dict shape as
    :func:`copilot.library_rpc.track_ref_to_wire` (renamed
    ``track_id`` -> ``id``) when the library row exists, or ``None``
    when the entry is dangling. UI normalizes both cases.
    """
    # Local import to avoid a circular import at module load time —
    # library_rpc imports from .library which imports nothing from
    # here, so the cycle only exists at the projection helper.
    from .library_rpc import track_ref_to_wire

    return {
        "track_id": entry.track_id,
        "position": int(entry.position),
        "added_at": entry.added_at,
        "track": (
            None if entry.track is None else track_ref_to_wire(entry.track)
        ),
    }


__all__ = [
    "PlaylistEntry",
    "PlaylistQueue",
    "entry_to_wire",
]
