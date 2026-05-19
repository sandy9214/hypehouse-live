"""Smoke test for ``scripts.bake_in.generate_tracks``.

The bake-in is the integration test; this just verifies the CLI parses
+ produces a manifest with the expected wire shape on a tiny catalog.
Anything larger belongs in the bake-in workflow itself.
"""
from __future__ import annotations

import json
import sys
import wave
from pathlib import Path

import pytest

# Make repo root importable so ``scripts.bake_in.*`` resolves under
# `pytest scripts/bake_in/tests`. The conftest sibling does the same
# thing for the run_set + verify tests.
sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from scripts.bake_in import generate_tracks  # noqa: E402


def test_cli_writes_manifest_and_wavs(tmp_path: Path) -> None:
    """End-to-end: ``python -m scripts.bake_in.generate_tracks`` produces N WAVs."""
    rc = generate_tracks.main(
        [
            "--out-dir",
            str(tmp_path),
            "--count",
            "3",
            "--duration-min",
            "0.05",  # ~3 s of catalog total → 1 s per track minimum
            "--seed",
            "42",
            "--log-level",
            "WARNING",
        ]
    )
    assert rc == 0

    manifest_path = tmp_path / "manifest.json"
    assert manifest_path.exists()
    manifest = json.loads(manifest_path.read_text())
    assert manifest["schema"] == 1
    assert len(manifest["tracks"]) == 3
    for track in manifest["tracks"]:
        wav = Path(track["path"])
        assert wav.exists()
        # WAV header is 44 bytes; we expect real audio data on top.
        assert wav.stat().st_size > 1024
        with wave.open(str(wav), "rb") as wf:
            assert wf.getnchannels() == 2
            assert wf.getframerate() == generate_tracks.SAMPLE_RATE_HZ
        assert generate_tracks.BPM_MIN <= track["bpm"] <= generate_tracks.BPM_MAX
        assert track["camelot_key"] in generate_tracks.CAMELOT_KEYS


def test_planner_rejects_bad_inputs() -> None:
    """Negative counts / durations fail loudly rather than spinning forever."""
    with pytest.raises(ValueError):
        generate_tracks.plan_catalog(count=0, duration_min=1.0, seed=0)
    with pytest.raises(ValueError):
        generate_tracks.plan_catalog(count=1, duration_min=0.0, seed=0)
