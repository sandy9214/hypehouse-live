"""Tests for scripts/cloud_sync_status.py.

Subprocess-driven so the test exercises the actual CLI surface
(argparse + exit codes + JSON emission). Builds a tiny SQLite
fixture per test using stdlib only — no need to bring up the full
TrackLibrary class.
"""
from __future__ import annotations

import json
import sqlite3
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT = REPO_ROOT / "scripts" / "cloud_sync_status.py"


def _make_db(
    path: Path, tracks: int, pending: int, with_pending_table: bool = True
) -> None:
    conn = sqlite3.connect(path)
    try:
        conn.execute(
            "CREATE TABLE tracks (track_id TEXT PRIMARY KEY, path TEXT)"
        )
        for i in range(tracks):
            conn.execute(
                "INSERT INTO tracks (track_id, path) VALUES (?, ?)",
                (f"t{i}", f"/m/t{i}.mp3"),
            )
        if with_pending_table:
            conn.execute(
                "CREATE TABLE pending_push ("
                "track_id TEXT PRIMARY KEY, queued_at_micros INTEGER)"
            )
            for i in range(pending):
                conn.execute(
                    "INSERT INTO pending_push "
                    "(track_id, queued_at_micros) VALUES (?, ?)",
                    (f"t{i}", 1_700_000_000_000_000 + i),
                )
        conn.commit()
    finally:
        conn.close()


def test_human_output(tmp_path: Path) -> None:
    db = tmp_path / "lib.db"
    _make_db(db, tracks=12, pending=3)
    result = subprocess.run(
        [sys.executable, str(SCRIPT), str(db)],
        capture_output=True,
        text=True,
        check=True,
    )
    assert result.stdout.strip() == "12 tracks, 3 pending push"


def test_json_output(tmp_path: Path) -> None:
    db = tmp_path / "lib.db"
    _make_db(db, tracks=5, pending=2)
    result = subprocess.run(
        [sys.executable, str(SCRIPT), str(db), "--json"],
        capture_output=True,
        text=True,
        check=True,
    )
    parsed = json.loads(result.stdout)
    assert parsed == {"library_track_count": 5, "pending_push_count": 2}


def test_missing_db_exits_2(tmp_path: Path) -> None:
    result = subprocess.run(
        [sys.executable, str(SCRIPT), str(tmp_path / "no_such.db")],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 2
    assert "library DB not found" in result.stderr


def test_pre_v11_schema_treats_missing_table_as_zero(
    tmp_path: Path,
) -> None:
    """Libraries that pre-date the cloud-sync v11 migration don't
    have a `pending_push` table. The script must report 0 pending
    rather than crashing."""
    db = tmp_path / "lib.db"
    _make_db(db, tracks=7, pending=0, with_pending_table=False)
    result = subprocess.run(
        [sys.executable, str(SCRIPT), str(db)],
        capture_output=True,
        text=True,
        check=True,
    )
    assert result.stdout.strip() == "7 tracks, 0 pending push"


def test_env_var_fallback(tmp_path: Path) -> None:
    db = tmp_path / "lib.db"
    _make_db(db, tracks=3, pending=1)
    result = subprocess.run(
        [sys.executable, str(SCRIPT)],
        capture_output=True,
        text=True,
        env={
            "HYPEHOUSE_LIBRARY_DB": str(db),
            "PATH": "/usr/bin:/bin",
        },
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "3 tracks, 1 pending push"
