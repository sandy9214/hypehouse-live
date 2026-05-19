"""Startup wiring for the cloud library sync (#102 slice 3).

Builds a `SyncClient` from process env at copilot startup and runs a
single bootstrap pull. The result is logged + returned so callers can
choose to short-circuit further wiring (e.g. UI badge "cloud sync off"
when creds are missing).

Slice 3 ships pull-only + read-only against the local library — no
local writes yet (the merger lands once `TrackLibrary` grows
`updated_at_micros` columns in slice 4). The bootstrap pull proves
the live wire works end-to-end + populates the in-process syncer
state for the next slice's library-merge pass.
"""

from __future__ import annotations

import logging
import os
from typing import Optional

from .client import InMemorySyncClient, SyncClient, SyncError
from .supabase import SupabaseSyncClient
from .syncer import LibrarySyncer, PullResult


def build_sync_client_from_env(
    *,
    logger: Optional[logging.Logger] = None,
) -> SyncClient:
    """Return the best available `SyncClient` for the live process.

    Decision tree:
      * `SUPABASE_URL` + `SUPABASE_ANON_KEY` both set → `SupabaseSyncClient`
        (live cloud).
      * Either env var missing → `InMemorySyncClient` (local-only mode;
        sync ops are no-ops as far as remote storage is concerned, but
        the syncer can still pull from a manually-seeded fake during
        tests / dev).

    Never raises. Always returns a usable client so the copilot
    startup path can proceed even when the operator has not yet
    provisioned cloud creds.
    """
    log = logger or logging.getLogger("copilot.cloud_sync.bootstrap")
    url = (os.environ.get("SUPABASE_URL") or "").strip()
    key = (os.environ.get("SUPABASE_ANON_KEY") or "").strip()
    if not url or not key:
        log.info(
            "cloud sync: SUPABASE_URL / SUPABASE_ANON_KEY not set — "
            "using in-memory fallback (local-only mode)"
        )
        return InMemorySyncClient()
    try:
        client = SupabaseSyncClient(url=url, anon_key=key)
        log.info("cloud sync: SupabaseSyncClient ready (url=%s)", url)
        return client
    except SyncError as e:
        log.warning(
            "cloud sync: SupabaseSyncClient construction failed (%s) — "
            "using in-memory fallback",
            e,
        )
        return InMemorySyncClient()


def bootstrap_pull(
    client: SyncClient,
    *,
    logger: Optional[logging.Logger] = None,
    since_micros: int = 0,
) -> PullResult:
    """Run one pull and log a per-outcome summary.

    Read-only against the local library — `apply_remote` is a no-op
    stub today. Slice 4 wires the actual library merge once
    `TrackLibrary` learns `updated_at_micros`. We still pull so the
    operator sees the cloud row count in the startup log and can
    confirm the wire is alive.
    """
    log = logger or logging.getLogger("copilot.cloud_sync.bootstrap")
    syncer = LibrarySyncer(
        client,
        local_updated_at=lambda _id: None,  # forces every row into LOCAL_INSERTED
        apply_remote=lambda _row: None,  # no-op until library learns updated_at_micros
    )
    result = syncer.pull(since_micros=since_micros)
    if result.transport_error is not None:
        log.warning(
            "cloud sync: bootstrap pull transport error: %s",
            result.transport_error,
        )
    else:
        log.info(
            "cloud sync: bootstrap pull ok — fetched=%d inserted=%d applied=%d kept_local=%d",
            result.fetched_count,
            result.inserted_count,
            result.applied_count,
            result.kept_local_count,
        )
    return result


__all__ = ["bootstrap_pull", "build_sync_client_from_env"]
