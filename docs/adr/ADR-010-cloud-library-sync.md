# ADR-010 — Cloud library sync (Supabase + stdlib)

**Status**: Accepted 2026-05-19. Implementation shipped across #102
(slices 1-6, #148-#153) plus the polish tail #155-#202.
**Decider**: Sandeep Gorai
**Trigger**: Closing issue #102 — operator wants the same track
catalog visible on every machine they use, with conflict-safe
ingest from any of them.

## Context

Hypehouse-live's local catalog (`copilot/library.py` →
`~/.hypehouse-live/library.db`) was machine-local. Operators with a
desk laptop + a stage laptop had to manually rsync the SQLite file,
which:

- broke if both machines added tracks before the rsync (one side's
  edits clobbered the other's),
- exposed no progress signal — operators couldn't tell if a sync
  was in-flight or stuck,
- required out-of-band tooling (`rsync` / `scp` / Dropbox / iCloud)
  that hypehouse-live couldn't reason about.

The fix needed three properties:

1. **Conflict-safe**: edits from two machines in a short window
   shouldn't silently destroy one of the writes.
2. **Async / non-blocking**: the operator shouldn't wait for cloud
   round-trips to add a track.
3. **No new heavy dependencies**: adding `psycopg`, `supabase-py`,
   or any C-extension would expand the v0.x install surface (we
   ship a Tauri binary; every dep increases the build matrix).

## Decision

### Storage: Supabase, schema mirrors the local SQLite tracks table

- Single `tracks` table with the same column set as the local
  catalog (track_id PK, path, bpm, camelot_key, energy, duration_s,
  hot_cues_ms bigint[], updated_at_micros bigint NOT NULL).
- Indexed on `updated_at_micros` so the sync watermark filter
  (`updated_at_micros >= since_micros`) is an index range scan.
- RLS OFF by default (single-user v0.x). Multi-user flips the
  policy at the bottom of `001_tracks.sql`.
- Migration shipped as a plain `.sql` file. `make supabase-print`
  emits it with paste-ready setup instructions for operators who
  don't have the supabase CLI installed.

### Transport: PostgREST + Python stdlib `urllib`

- No `supabase-py` dependency. `SupabaseSyncClient` is ~150 lines
  of stdlib `urllib.request` wrapping PostgREST's REST surface.
- Auth: anon key in the `apikey` + `Authorization: Bearer ...`
  headers. Anon key is safe to ship in the desktop binary — with
  RLS on, the key can only do what the policies allow; with RLS
  off (single-user mode), the key can read/write the table, and
  the operator is the only user.
- Idempotent push via PostgREST's `Prefer: resolution=merge-duplicates`
  + `?on_conflict=track_id` — POSTing the same row twice doesn't
  duplicate it.

### Conflict resolution: last-write-wins on `updated_at_micros`

- Every local write stamps `updated_at_micros = time.time() *
  1_000_000`. The pull path takes the remote row only when the
  remote's micros are strictly greater than the local micros;
  otherwise the local wins and the syncer marks the row for
  outbound push.
- Trade-off accepted: two machines editing the same track within
  the same wall-clock micro lose one edit. Acceptable for v0.x —
  the conflict surface is "I edited the same track on two
  machines in the same second," which is rare for the target user
  (one operator with multiple machines, not a collaborative team).

### Outbound queue: separate `pending_push` table

- Pulled out of the `tracks` table itself so that:
  - the push state doesn't leak into the queryable catalog,
  - re-running the daemon doesn't lose queue state on restart,
  - the operator can explicitly re-enqueue via
    `library.requeue_all_pending` after upgrades (#179) — a
    `INSERT OR IGNORE INTO pending_push SELECT ...` over the
    catalog table is the entire implementation.
- `add_track` enqueues; `upsert_from_remote` does NOT (would
  cause a bounce loop where the remote keeps re-pushing what it
  just pulled).

### Background daemon with exponential backoff (#169) + wake-on-demand (#176)

- `SyncDaemon` runs `tick_once` every `HYPEHOUSE_SYNC_TICK_SECONDS`
  (default 60s). On consecutive transport / SQLite errors, the
  wait doubles per failure, capped at `MAX_BACKOFF_SECONDS`
  (10 min). First clean tick resets.
- `_stop` event for shutdown + separate `_wake` event for "operator
  clicked sync-now, refresh the schedule now." `wake_now(*,
  skip_next_tick=True)` controls whether the daemon re-runs
  `tick_once` after waking — `library.sync_now` passes True (RPC
  already ran a manual tick); `library.requeue_all_pending` passes
  False (no manual tick happened, daemon must actually drain).
- Counter mutations (`_consecutive_failures`,
  `_skip_next_auto_tick`) and stats snapshots are lock-protected
  (`_stats_lock`) because `library.sync_now` calls `tick_once`
  from the RPC handler thread, while the daemon loop calls it
  from its own thread.

### `next_sync_micros` owned by `_loop`, not `tick_once`

- The daemon stamps `next_sync_micros = now + next_wait_seconds()`
  immediately before `_wake.wait(...)` in `_loop`. `tick_once`
  preserves whatever the loop last stamped.
- Without this, an out-of-band `tick_once` (via `sync_now`) would
  advertise a fresh "next in 60s" while the daemon thread was
  still asleep on a much longer backoff window (Codex caught this
  in #174 R1 review).

## Alternatives considered

- **`supabase-py` SDK**. Would shorten `SupabaseSyncClient` to
  ~30 lines but pulls in `httpx` + `pydantic` + `realtime-py` etc.
  Not worth it for a HTTP-only client.
- **Direct Postgres via the connection pooler** (`psycopg` or
  `asyncpg`). Lower-latency than PostgREST but requires shipping
  Postgres credentials in the desktop binary, plus the C-ext
  build complications.
- **CRDT (e.g. Yjs) instead of LWW**. Real merge semantics, but
  too heavy for a v0.x where the conflict surface is rare and the
  data shape is flat.
- **Polling instead of subscribing to Postgres LISTEN/NOTIFY**.
  We poll. Notifications would be lower-latency but introduce
  WebSocket lifecycle complexity — backoff, reconnect, multi-tab,
  etc. The 60s poll is sufficient for a track-catalog sync.

## Consequences

### Positive

- Single SQL file (`001_tracks.sql`) + env vars + restart =
  working sync. Operator setup documented in `docs/cloud-sync.md`.
- No new heavy dependencies — `pip install -e copilot/` doesn't
  drag in anything compiled.
- The 5 RPC methods (`sync_status`, `sync_now`, `list_pending_push`,
  `requeue_all_pending`, `stems_status`) + AboutPanel surface
  (last-sync row + countdown + sync-now button + queue-all button
  + per-row chip + Library "Pending sync" filter) give operators a
  full mental model without needing to tail logs.
- Ops monitoring via `make cloud-sync-status` (#189) — cron /
  launchd can alert when the queue stops draining without the UI
  open.

### Negative

- LWW conflicts silently lose one edit when two machines write
  the same track in the same wall-clock micro. Mitigation: the
  pending-push queue is observable (`library.list_pending_push`
  + UI chip + filter), so an operator who suspects a conflict
  can manually verify.
- Anon-key safety relies on RLS being on for multi-user mode —
  the v0.x default of RLS OFF is documented in
  `docs/known-limitations.md` and `001_tracks.sql` as a single-user
  caveat. Flipping to multi-user requires a schema change + sign-in
  flow we haven't built.
- Pre-v10 local catalogs aren't auto-enqueued on upgrade — the
  operator needs to click "queue all" once (#181) or call
  `library.requeue_all_pending` (#179). The migration could have
  done it automatically, but auto-enqueueing 10k tracks on first
  boot would burn cloud quota without operator awareness.

## Related issues / PRs

- Issue #102 (umbrella) — closed.
- #148-#153 (slices 1-6) — initial scaffolding through daemon.
- #155, #157, #159, #161, #163, #165, #167, #169 — UI surface +
  daemon stats + backoff.
- #171 — operator setup guide (`docs/cloud-sync.md`).
- #174 — `next_sync_micros` countdown (with the Codex-caught
  ownership fix).
- #176, #179, #181, #184 — wake_now contract evolution.
- #187 — Library "Pending sync" filter.
- #189 — `scripts/cloud_sync_status.py` ops CLI.
- #190, #199, #200, #202 — docs refresh tail.
- #195, #197 — stems-status RPC + UI (adjacent, not strictly
  cloud-sync, but uses the same hook plumbing).
