#!/usr/bin/env python3
"""Emit cloud-sync schema migrations + paste-ready setup steps.

Usage: ``python scripts/print_supabase_migrations.py``

Most operators don't have the Supabase CLI installed. This script
prints every ``.sql`` file under
``copilot/cloud_sync/migrations/`` in numeric order, separated by a
banner that names the file, and finishes with a short instruction
block so the output can be eyeballed + pasted straight into the
Supabase SQL editor. No third-party deps; stdlib only.
"""
from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MIGRATIONS_DIR = REPO_ROOT / "copilot" / "cloud_sync" / "migrations"


def main() -> int:
    files = sorted(MIGRATIONS_DIR.glob("*.sql"))
    if not files:
        print(
            f"No .sql files found under {MIGRATIONS_DIR}",
            file=sys.stderr,
        )
        return 1
    out: list[str] = []
    for path in files:
        out.append("")
        out.append(f"-- ====== {path.name} ======")
        out.append(path.read_text())
    out.append("")
    out.append("-- ---")
    out.append("-- Setup:")
    out.append("--   1. Open the Supabase SQL editor (Project → SQL).")
    out.append("--   2. Paste the SQL above and click Run.")
    out.append("--   3. Export env vars before restarting the co-pilot:")
    out.append("--        export SUPABASE_URL=https://YOUR-REF.supabase.co")
    out.append("--        export SUPABASE_ANON_KEY=eyJhbGciOi...")
    out.append("--   4. See docs/cloud-sync.md for the full operator guide.")
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
