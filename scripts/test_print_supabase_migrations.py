"""Smoke test for scripts/print_supabase_migrations.py.

The script is so simple that this test mostly exists to flag if a
future migration file gets dropped on the floor (the loop only picks
up `*.sql` — a typo like `001_tracks.SQL` would silently skip).
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT = REPO_ROOT / "scripts" / "print_supabase_migrations.py"
MIGRATIONS_DIR = REPO_ROOT / "copilot" / "cloud_sync" / "migrations"


def test_print_emits_every_sql_file_with_banner() -> None:
    result = subprocess.run(
        [sys.executable, str(SCRIPT)],
        capture_output=True,
        text=True,
        check=True,
    )
    out = result.stdout
    sql_files = sorted(MIGRATIONS_DIR.glob("*.sql"))
    assert sql_files, "migrations dir unexpectedly empty"
    for path in sql_files:
        assert f"-- ====== {path.name} ======" in out, (
            f"banner for {path.name} missing from output"
        )
    # Setup hint must appear so an operator who pipes this to pbcopy
    # also gets the env-var instructions.
    assert "SUPABASE_URL=" in out
    assert "SUPABASE_ANON_KEY=" in out
    assert "docs/cloud-sync.md" in out
