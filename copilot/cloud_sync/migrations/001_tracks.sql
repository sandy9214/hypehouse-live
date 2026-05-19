-- Supabase schema for the hypehouse-live cloud library sync (#102).
-- Apply via the Supabase SQL editor or `supabase db push`.

create table if not exists tracks (
  track_id          text primary key,
  path              text not null,
  bpm               double precision not null,
  camelot_key       text not null,
  energy            double precision not null,
  duration_s        double precision not null,
  -- 8-element array; -1 sentinel marks "empty slot" since Postgres
  -- arrays can't carry sparse nulls without an OPP enum layer.
  hot_cues_ms       bigint[] not null default array[-1,-1,-1,-1,-1,-1,-1,-1],
  updated_at_micros bigint not null default 0
);

-- Index the last-write-wins watermark so the syncer's
-- `updated_at_micros >= since_micros` filter runs as an index range
-- scan instead of a sequential scan.
create index if not exists tracks_updated_at_idx
  on tracks (updated_at_micros);

-- Row-Level Security — leave OFF for v0.x single-user mode. When we
-- ship multi-user (issue TBD), add:
--   alter table tracks enable row level security;
--   create policy "owner can rw" on tracks for all using (auth.uid() = owner_id);
-- ...plus an `owner_id uuid not null default auth.uid()` column.
