#!/usr/bin/env python3
"""Print cloud-sync queue status for ops monitoring.

Usage:
  python scripts/cloud_sync_status.py [PATH_TO_LIBRARY_DB]
  python scripts/cloud_sync_status.py --json

Stdlib only — opens the SQLite library DB read-only and prints the
catalog size + pending-push queue depth. Exit 0 on success, 2 if the
DB doesn't exist, 3 on a SQLite error. Useful for cron / launchd
agents that want to alert when the queue stops draining (e.g. cloud
outage) without round-tripping through the running co-pilot service.

The DB path defaults to ``$HYPEHOUSE_LIBRARY_DB`` and falls back to
``~/.hypehouse-live/library.db`` to match
``copilot/main.py`` + ``copilot/library.py``.
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
import sys
from pathlib import Path


DEFAULT_LIBRARY_DB = Path("~/.hypehouse-live/library.db").expanduser()


def resolve_db_path(arg: str | None) -> Path:
    if arg:
        return Path(arg).expanduser()
    env = os.environ.get("HYPEHOUSE_LIBRARY_DB", "").strip()
    if env:
        return Path(env).expanduser()
    return DEFAULT_LIBRARY_DB


def read_status(db_path: Path) -> dict[str, int]:
    """Read the two counts off the SQLite DB.

    Opens the connection in read-only `mode=ro` so a misbehaving
    script can never corrupt the live library. Closes the connection
    even on SQLite errors.
    """
    uri = f"file:{db_path}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    try:
        # `pending_push` may not exist on a pre-v11 schema; treat ONLY
        # the missing-table case as an empty queue so the script keeps
        # working on libraries that pre-date cloud sync. Any other
        # OperationalError (busy locks, corruption, malformed schema)
        # re-raises so the operator gets exit-3 instead of a falsely
        # healthy "0 pending" reading (Codex review note on #189 R1).
        try:
            pending = conn.execute(
                "SELECT COUNT(*) FROM pending_push"
            ).fetchone()[0]
        except sqlite3.OperationalError as exc:
            if "no such table: pending_push" in str(exc):
                pending = 0
            else:
                raise
        tracks = conn.execute("SELECT COUNT(*) FROM tracks").fetchone()[0]
        return {
            "library_track_count": int(tracks),
            "pending_push_count": int(pending),
        }
    finally:
        conn.close()


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "db", nargs="?", default=None,
        help="path to library.db (default: $HYPEHOUSE_LIBRARY_DB "
        "or ~/.hypehouse-live/library.db)",
    )
    parser.add_argument(
        "--json", action="store_true",
        help="emit machine-readable JSON instead of the human summary",
    )
    args = parser.parse_args(argv)

    db_path = resolve_db_path(args.db)
    if not db_path.exists():
        print(
            f"library DB not found: {db_path}",
            file=sys.stderr,
        )
        return 2
    try:
        status = read_status(db_path)
    except sqlite3.Error as exc:
        print(f"sqlite error reading {db_path}: {exc}", file=sys.stderr)
        return 3
    if args.json:
        print(json.dumps(status, separators=(",", ":")))
    else:
        print(
            f"{status['library_track_count']} tracks, "
            f"{status['pending_push_count']} pending push"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
