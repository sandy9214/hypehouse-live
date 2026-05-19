"""Smoke test for ``scripts.bake_in.verify``.

Builds a tiny fake bake artifact set + run_report, runs the verify CLI
against it, asserts the green path returns 0 and the red path (missing
master.wav, decode-error event) flips to non-zero.
"""
from __future__ import annotations

import json
import struct
import sys
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from scripts.bake_in import verify  # noqa: E402


def _write_pcm_wav(path: Path, frames: int, peak: float = 0.5) -> None:
    """Write a stereo PCM16 WAV with a 1 kHz sine of ``frames`` length."""
    path.parent.mkdir(parents=True, exist_ok=True)
    import math

    sr = 44_100
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(2)
        wf.setsampwidth(2)
        wf.setframerate(sr)
        ints: list[int] = []
        for n in range(frames):
            v = peak * math.sin(2.0 * math.pi * 1000.0 * n / sr)
            pcm = int(max(-1.0, min(1.0, v)) * 32_767)
            ints.append(pcm)
            ints.append(pcm)
        wf.writeframes(struct.pack(f"<{len(ints)}h", *ints))


def _write_events(
    path: Path, transitions: int, include_decode_error: bool = False
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    lines: list[str] = []
    for i in range(transitions):
        lines.append(
            json.dumps(
                {
                    "id": i,
                    "kind": {"DeckLoad": {"deck": "A", "track": {}}},
                    "dropped_frames": 0,
                }
            )
        )
    if include_decode_error:
        lines.append(
            json.dumps({"id": 999, "kind": "engine.decode_error", "track_id": "x"})
        )
    path.write_text("\n".join(lines) + "\n")


def _setup_run(
    tmp_path: Path, *, transitions: int, include_decode_error: bool
) -> Path:
    session_dir = tmp_path / "sessions" / "sess-001"
    master_wav = session_dir / "master.wav"
    events_log = session_dir / "events.jsonl"
    telemetry_log = tmp_path / "telemetry.log"
    _write_pcm_wav(master_wav, frames=4096)
    _write_events(events_log, transitions, include_decode_error)
    telemetry_log.write_text("")

    report_path = tmp_path / "run_report.json"
    report_path.write_text(
        json.dumps(
            {
                "schema": 1,
                "playlist_len": max(1, transitions + 1),
                "rpc": {},
                "engine": {"returncode": 0},
                "copilot": {"returncode": 0},
                "artifacts": {
                    "session_dir": str(session_dir),
                    "master_wav": str(master_wav),
                    "events_log": str(events_log),
                    "telemetry_log": str(telemetry_log),
                    "manifest": str(tmp_path / "manifest.json"),
                },
                "error": None,
            }
        )
    )
    return report_path


def test_verify_green_path(tmp_path: Path) -> None:
    report = _setup_run(tmp_path, transitions=5, include_decode_error=False)
    rc = verify.main(["--report", str(report), "--log-level", "WARNING"])
    assert rc == 0


def test_verify_fails_on_decode_error(tmp_path: Path, capsys: object) -> None:
    report = _setup_run(tmp_path, transitions=5, include_decode_error=True)
    rc = verify.main(["--report", str(report), "--log-level", "WARNING"])
    assert rc != 0
    summary = json.loads(capsys.readouterr().out)  # type: ignore[attr-defined]
    assert summary["ok"] is False
    names = {c["name"]: c for c in summary["checks"]}
    assert names["no_decode_errors"]["passed"] is False


def test_verify_fails_on_missing_master(tmp_path: Path, capsys: object) -> None:
    report = _setup_run(tmp_path, transitions=5, include_decode_error=False)
    # Nuke the master.wav AND clear the artifacts reference so verify
    # reports a real missing-artifact failure rather than a stale path.
    payload = json.loads(report.read_text())
    Path(payload["artifacts"]["master_wav"]).unlink()
    payload["artifacts"]["master_wav"] = str(
        Path(payload["artifacts"]["session_dir"]) / "missing.wav"
    )
    report.write_text(json.dumps(payload))
    rc = verify.main(["--report", str(report), "--log-level", "WARNING"])
    assert rc != 0
    summary = json.loads(capsys.readouterr().out)  # type: ignore[attr-defined]
    names = {c["name"]: c for c in summary["checks"]}
    assert names["master_wav_exists"]["passed"] is False
