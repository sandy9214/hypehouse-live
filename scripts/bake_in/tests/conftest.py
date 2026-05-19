"""Pytest config for bake-in smoke tests.

Pins the repo root onto :data:`sys.path` so the tests can import the
``scripts.bake_in.*`` modules without a separate install step.
"""
from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))
