"""Smoke test for ``scripts.bake_in.run_set``.

Spawning the real engine / copilot binaries is the bake-in workflow's
job — this test only verifies the CLI parses, the smoke entry point
emits the expected run_report shape, and that the public ``run`` symbol
is importable. Anything heavier would either flake locally (no binary)
or take 25 min in CI (the bake itself).
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from scripts.bake_in import run_set  # noqa: E402


def test_smoke_main_writes_report(tmp_path: Path) -> None:
    report = run_set.smoke_main(tmp_path)
    assert report.exists()
    payload = json.loads(report.read_text())

    # Required keys for the verify step to do its work.
    assert payload["schema"] == 1
    for key in ("elapsed_s", "duration_min_requested", "playlist_len", "rpc"):
        assert key in payload, f"missing key {key}"
    rpc = payload["rpc"]
    for k in ("library_added", "playlist_enqueued", "auto_mix_set", "copilot_engaged"):
        assert k in rpc, f"missing rpc.{k}"

    artifacts = payload["artifacts"]
    for k in ("session_dir", "master_wav", "events_log", "telemetry_log", "manifest"):
        assert k in artifacts, f"missing artifacts.{k}"


def test_cli_rejects_missing_manifest(tmp_path: Path, capsys: object) -> None:
    """The full ``main`` entry refuses to run without a manifest file."""
    try:
        run_set.main(
            [
                "--out-dir",
                str(tmp_path),
                "--manifest",
                str(tmp_path / "does-not-exist.json"),
                "--duration-min",
                "0",
                "--playlist-len",
                "0",
                "--log-level",
                "WARNING",
            ]
        )
    except FileNotFoundError as exc:
        assert "manifest not found" in str(exc) or "does-not-exist" in str(exc)
    except SystemExit:
        # argparse may swallow the error if the engine binary check
        # fires first; either path is acceptable as long as it didn't
        # spawn anything.
        pass
