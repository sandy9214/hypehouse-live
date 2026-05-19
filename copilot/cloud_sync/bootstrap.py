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
from .syncer import LibrarySyncer, PullResult, PushResult


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
    library: object | None = None,
    logger: Optional[logging.Logger] = None,
    since_micros: int = 0,
) -> PullResult:
    """Run one pull and log a per-outcome summary.

    `library` is an optional `TrackLibrary` (typed as `object` here to
    avoid the import cycle; the actual contract is the two methods
    `local_updated_at_micros(track_id) -> int | None` and
    `upsert_from_remote(**row)`). When provided the syncer's
    `apply_remote` writes real local rows; when `None` the syncer runs
    in pull-only-log mode (slice 3 behaviour) so existing tests stay
    unchanged.
    """
    log = logger or logging.getLogger("copilot.cloud_sync.bootstrap")
    if library is None:
        local_updated_at = lambda _id: None  # noqa: E731
        apply_remote = lambda _row: None  # noqa: E731
    else:
        def local_updated_at(track_id: str) -> int | None:
            return library.local_updated_at_micros(track_id)  # type: ignore[attr-defined]

        def apply_remote(row) -> None:  # type: ignore[no-untyped-def]
            library.upsert_from_remote(  # type: ignore[attr-defined]
                track_id=row.track_id,
                path=row.path,
                bpm=row.bpm,
                camelot_key=row.camelot_key,
                energy=row.energy,
                duration_s=row.duration_s,
                hot_cues=row.hot_cues_as_options(),
                updated_at_micros=row.updated_at_micros,
            )

    syncer = LibrarySyncer(
        client,
        local_updated_at=local_updated_at,
        apply_remote=apply_remote,
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


def bootstrap_push(
    client: SyncClient,
    library: object,
    *,
    logger: Optional[logging.Logger] = None,
) -> PushResult:
    """Drain the library's outbound push queue once (#102 slice 5).

    Reads `library.pending_push_ids()`, asks `row_for_cloud_push()` for
    each, calls `client.upsert_track`, then `clear_pending_push()` per
    success. Transport error aborts the pass; remaining ids stay
    queued for the next pass.
    """
    log = logger or logging.getLogger("copilot.cloud_sync.bootstrap")
    syncer = LibrarySyncer(
        client,
        local_updated_at=lambda _id: None,
        apply_remote=lambda _row: None,
    )
    ids = library.pending_push_ids()  # type: ignore[attr-defined]
    if not ids:
        log.info("cloud sync: push queue empty")
        return PushResult(pushed_count=0, skipped_missing_count=0)
    result = syncer.push_pending(
        ids,
        row_loader=library.row_for_cloud_push,  # type: ignore[attr-defined]
        on_pushed=library.clear_pending_push,  # type: ignore[attr-defined]
    )
    if result.transport_error is not None:
        log.warning(
            "cloud sync: push transport error after %d pushed: %s",
            result.pushed_count,
            result.transport_error,
        )
    else:
        log.info(
            "cloud sync: push ok — pushed=%d skipped_missing=%d",
            result.pushed_count,
            result.skipped_missing_count,
        )
    return result


__all__ = ["bootstrap_pull", "bootstrap_push", "build_sync_client_from_env"]
