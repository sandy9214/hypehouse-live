"""hypehouse-live cloud library sync — Supabase-backed mirror.

Issue #102 scaffold (slice 1). The runtime client + REST calls are
behind the `SyncClient` protocol so unit tests can substitute a
deterministic in-memory fake; the real Supabase wiring lands in the
follow-up slice with the project's actual URL + anon key.

Layers
------

* :class:`SyncClient` — protocol; one method per remote operation.
* :class:`InMemorySyncClient` — test fake; used by every unit test in
  this module so we never depend on a live Supabase project for CI.
* :class:`SupabaseSyncClient` — REST adapter. Slice 2.
* :class:`LibrarySyncer` — orchestration. Pulls remote rows, resolves
  conflicts, emits local upserts. Pure(ish) — takes a callback for the
  local apply path so it can be wired into the existing
  ``copilot.library.Library`` without circular imports.

Schema
------

Single table ``tracks`` keyed by ``track_id``:

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
wins and the syncer marks the row for an outbound push (slice 2).

Slice 1 ships pull-only — the outbound queue is stubbed.
"""

from .client import (
    InMemorySyncClient,
    RemoteTrack,
    SyncClient,
    SyncError,
)
from .daemon import DEFAULT_TICK_SECONDS, SyncDaemon
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
]
