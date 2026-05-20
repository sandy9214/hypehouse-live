"""hypehouse-live cloud library sync — Supabase-backed mirror.

Closes issue #102 (full pull + push + daemon + UI surface; see
``docs/cloud-sync.md`` for the operator setup guide).

Layers
------

* :class:`SyncClient` — Protocol; one method per remote operation.
* :class:`InMemorySyncClient` — test fake; used by every unit test in
  this module so we never depend on a live Supabase project for CI.
* :class:`SupabaseSyncClient` — PostgREST adapter (stdlib urllib, no
  third-party SDK). Constructed via :meth:`SupabaseSyncClient.from_env`
  from ``$SUPABASE_URL`` + ``$SUPABASE_ANON_KEY``.
* :class:`LibrarySyncer` — orchestration. Pulls remote rows, resolves
  conflicts via last-write-wins, drains the local ``pending_push``
  queue. Pure(ish) — callbacks for the local apply / clear paths so it
  composes with :class:`copilot.library.TrackLibrary` without
  circular imports.
* :class:`SyncDaemon` — background thread. Calls ``tick_once`` on a
  fixed cadence (``HYPEHOUSE_SYNC_TICK_SECONDS``, default 60s) with
  exponential backoff on consecutive failures, capped at
  :data:`MAX_BACKOFF_SECONDS` (10 min). ``wake_now`` lets RPC handlers
  short-circuit a long backoff wait after manual sync work or newly
  queued push work — ``library.sync_now`` (after its out-of-band
  ``tick_once``) calls ``wake_now(skip_next_tick=True)`` so the
  daemon skips a redundant duplicate tick, while
  ``library.requeue_all_pending`` (no manual tick) calls
  ``wake_now(skip_next_tick=False)`` so the daemon's next iteration
  actually drains the freshly enqueued rows.

Schema
------

Single table ``tracks`` keyed by ``track_id``
(``copilot/cloud_sync/migrations/001_tracks.sql`` — paste into the
Supabase SQL editor, or use ``make supabase-print | pbcopy``):

* ``track_id TEXT PRIMARY KEY``
* ``path TEXT NOT NULL``
* ``bpm DOUBLE PRECISION NOT NULL``
* ``camelot_key TEXT NOT NULL``
* ``energy DOUBLE PRECISION NOT NULL``
* ``duration_s DOUBLE PRECISION NOT NULL``
* ``hot_cues_ms BIGINT[]`` — 8-element array, ``-1`` for unset slots
  (Supabase JSON can't carry a sparse array natively; ``-1`` sentinel
  collapses cleanly through ``int|None``).
* ``updated_at_micros BIGINT NOT NULL`` — wall-clock micros at write
  time. Used by the syncer for last-write-wins conflict resolution.
  Indexed (``tracks_updated_at_idx``) so the ``since_micros`` filter
  on pull is an index range scan.

Local-side schema mirror lives in :mod:`copilot.library`:

* ``tracks.updated_at_micros`` (schema v10) — wall-clock micros so
  the local row can win a conflict against an older remote.
* ``pending_push`` table (schema v11) — outbound queue. ``add_track``
  stamps a row here; the daemon drains it.

Future tables (deferred):

* ``hot_cues`` — moved out of the row into its own table once we
  outgrow the 8-slot fixed grid.
* ``stems`` — pointer rows for cloud-rendered stem assets.
* ``presets`` — cloud-shareable preset snapshots.

Conflict resolution
-------------------

Last-write-wins on ``updated_at_micros``. The syncer never blindly
overwrites a newer local row; if the local row's recorded
``updated_at_micros`` is greater than the remote's, the local row
wins and the daemon's next tick drains the row from ``pending_push``
into the remote.

RLS / multi-user
----------------

Row-Level Security is OFF in ``001_tracks.sql`` — fine for single-user
v0.x. Multi-user mode (not yet shipped) needs an ``owner_id`` column
plus the RLS policy at the bottom of the migration file.
"""

from .client import (
    InMemorySyncClient,
    RemoteTrack,
    SyncClient,
    SyncError,
)
from .daemon import DEFAULT_TICK_SECONDS, SyncDaemon, SyncStats
from .supabase import SupabaseSyncClient
from .syncer import ConflictOutcome, LibrarySyncer, PullResult, PushResult

__all__ = [
    "ConflictOutcome",
    "DEFAULT_TICK_SECONDS",
    "InMemorySyncClient",
    "LibrarySyncer",
    "PullResult",
    "PushResult",
    "RemoteTrack",
    "SupabaseSyncClient",
    "SyncClient",
    "SyncDaemon",
    "SyncError",
    "SyncStats",
]
