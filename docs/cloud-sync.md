# Cloud library sync — operator setup

Wire the co-pilot's `TrackLibrary` to a Supabase project so a track
added on one machine shows up on another within ~60 seconds. Pull /
push is last-write-wins on `updated_at_micros`; the daemon backs off
exponentially on consecutive transport errors (cap = 10 min).

## What ships

- `copilot/cloud_sync/` — `SupabaseSyncClient` (PostgREST + stdlib
  urllib), `LibrarySyncer` (last-write-wins resolver), `SyncDaemon`
  (background tick loop)
- `copilot/cloud_sync/migrations/001_tracks.sql` — Postgres schema
- `library.sync_status` + `library.sync_now` +
  `library.list_pending_push` + `library.requeue_all_pending`
  JSON-RPC methods (UI consumes these via AboutPanel + TrackRow chip)
- `make supabase-print` — convenience helper that emits the migration
  SQL with paste-ready setup instructions

## One-time setup

1. Create a Supabase project at <https://supabase.com>. Free tier is
   fine — the schema has one small table.
2. Copy `Project URL` and `anon public` key from `Settings → API`.
   The anon key is safe to ship in a desktop binary; PostgREST
   enforces row-level checks once you turn RLS on (see below).
3. Run the schema migration. Two options:
   - **Supabase SQL editor** (no CLI needed) — paste the contents of
     `copilot/cloud_sync/migrations/001_tracks.sql` and click Run.
   - **`supabase` CLI** (if installed + linked):
     ```sh
     supabase db push --file copilot/cloud_sync/migrations/001_tracks.sql
     ```
4. Export the env vars (also accepted on the launchd plist / systemd
   unit):
   ```sh
   export SUPABASE_URL=https://YOUR-REF.supabase.co
   export SUPABASE_ANON_KEY=eyJhbGciOi…
   ```
5. Restart the co-pilot service. The daemon starts on the next boot
   and pulls + pushes once at start, then every
   `HYPEHOUSE_SYNC_TICK_SECONDS` (default 60s).

## Verifying the wire-up

- `AboutPanel` → "Last sync" row shows `Xs ago · next in Xs` after
  the first tick.
- After importing a track, the same row shows `· N pending sync`
  briefly, then drops to zero once the next tick drains the queue.
- Track rows in the Library panel show a `⟳ pending` chip while the
  push is queued. The **"Pending sync"** checkbox in the library
  filter bar narrows the visible rows to just the pending set
  (active filter renders a removable `pending sync only` chip).
- AboutPanel **"sync now"** button → immediate `library.sync_now`
  tick. The daemon also wakes so the next automatic tick fires at
  the reset cadence.
- AboutPanel **"queue all"** button → fires
  `library.requeue_all_pending` (operator escape hatch after a
  pre-cloud-sync upgrade — seeds the cloud from an existing local
  library).

## Ops monitoring

For headless monitoring (cron / launchd) without the UI:

```sh
make cloud-sync-status            # human: "12 tracks, 3 pending push"
make cloud-sync-status DB=...     # override path
python scripts/cloud_sync_status.py --json   # machine output
```

Exit 0 / 2 (DB missing) / 3 (sqlite error). See
`scripts/cloud_sync_status.py` for the full surface.

## Things to know

- **Backoff:** on transport / DB errors the daemon doubles its wait
  each tick up to 10 minutes. First clean tick resets to the base
  cadence.
- **RLS is OFF by default** — fine for single-user. For multi-user
  mode the migration file has commented-out SQL at the bottom.
- **Conflict resolution** is last-write-wins on `updated_at_micros`.
  No merge; the higher wall-clock value wins. SQLite locally + the
  daemon are the only two writers, and the daemon always stamps the
  remote micros — so the only conflict surface is "same track edited
  on two machines within the same 60s window."
- **Anon key safety**: the anon key on its own can only do what RLS
  policies allow. With RLS off (single-user) it can read + write the
  table — accept that and don't ship multi-tenant builds with RLS
  off.
