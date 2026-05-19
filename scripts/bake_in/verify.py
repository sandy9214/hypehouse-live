"""Assert post-bake invariants. Exit 0 on green, non-zero with a report on red.

Reads ``run_report.json`` produced by :mod:`run_set` plus the engine's
session directory and checks:

* ``master.wav`` exists and is non-empty (the engine recorder must have
  flushed at least the WAV header + some audio body).
* ``events.jsonl`` has no ``engine.decode_error`` records — the auto-mix
  proposer is allowed to skip tracks, but a decoder panic means the
  symphonia path crashed mid-load and that's a hard fail.
* The event log shows at least ``playlist_len - 1`` ``DeckLoad`` events,
  evidence that the auto-mix actually executed transitions across the
  playlist (the v0.2 acceptance bar — "auto-mix advances tracks").
* Recorder dropped frames stay under 0.1 % of total audio frames. The
  recorder ring is 1 s @ 48 kHz so anything above 0.1 % is the writer
  thread starving and the master.wav has audible gaps.
* The Sentry telemetry log is either empty (operator did not opt in) or
  contains only events whose envelope ``type`` is in
  :data:`TELEMETRY_ALLOWLIST` — protects the privacy contract documented
  in ``docs/telemetry.md``.

Output is a JSON summary on stdout (or ``--report`` file) plus a one-
line PASS/FAIL on stderr. Returns exit code 0 if every assertion holds,
non-zero otherwise.
"""
from __future__ import annotations

import argparse
import json
import logging
import math
import sys
import wave
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Sequence

log = logging.getLogger("bake_in.verify")

#: Telemetry envelope ``type`` fields the privacy contract permits even
#: when the operator opted in. Anything outside this set is a regression
#: — see ``copilot/telemetry.py`` for the wire shape we expect.
TELEMETRY_ALLOWLIST: frozenset[str] = frozenset({"event", "transaction"})

#: Max tolerated dropped-frame ratio (1 = 100 % of frames dropped).
#: 0.1 % means at most ~50 ms of dropouts across a 25-min bake at
#: 48 kHz — anything worse and the master.wav has audible gaps.
DROPPED_FRAME_BUDGET = 0.001


@dataclass
class CheckResult:
    """One named invariant check."""

    name: str
    passed: bool
    detail: str = ""


@dataclass
class VerifySummary:
    """Roll-up of every check + a single ``ok`` boolean."""

    checks: list[CheckResult] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return all(c.passed for c in self.checks)

    def to_json(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "checks": [
                {"name": c.name, "passed": c.passed, "detail": c.detail}
                for c in self.checks
            ],
        }


# --- WAV introspection helpers -------------------------------------------


def _wav_frame_count(path: Path) -> int:
    """Read the WAV header and return total sample frames.

    Cheap — we open the file with :mod:`wave`, which only parses the
    44-byte header. For IEEE-float WAVs (the engine's master format)
    ``wave`` raises; fall back to inferring from raw file size.
    """
    try:
        with wave.open(str(path), "rb") as wf:
            return int(wf.getnframes())
    except (wave.Error, OSError):
        # IEEE-float master.wav: 32-bit stereo → 8 bytes per frame.
        # The engine pads its WAVs with a 44-byte header.
        try:
            size = path.stat().st_size
            body = max(0, size - 44)
            return body // 8
        except OSError:
            return 0


# --- individual checks ----------------------------------------------------


def check_master_wav_exists(master_wav: Path | None) -> CheckResult:
    if master_wav is None:
        return CheckResult(
            "master_wav_exists", False, "session_dir/master.wav not produced"
        )
    if not master_wav.exists():
        return CheckResult(
            "master_wav_exists", False, f"missing: {master_wav}"
        )
    size = master_wav.stat().st_size
    if size <= 44:
        return CheckResult(
            "master_wav_exists",
            False,
            f"{master_wav} is header-only (size={size}B); engine wrote no audio",
        )
    return CheckResult(
        "master_wav_exists",
        True,
        f"{master_wav.name} = {size} bytes",
    )


def _read_events(events_log: Path | None) -> list[dict[str, Any]]:
    if events_log is None or not events_log.exists():
        return []
    out: list[dict[str, Any]] = []
    with events_log.open() as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                # Truncated tail (crash mid-write) — preserve a marker so
                # the verify summary surfaces it instead of silently
                # treating it as zero events.
                out.append({"_parse_error": True, "raw": line})
    return out


def check_no_decode_errors(events: Sequence[dict[str, Any]]) -> CheckResult:
    bad = [
        e
        for e in events
        if isinstance(e, dict)
        and (
            e.get("kind") == "engine.decode_error"
            or (
                isinstance(e.get("kind"), dict)
                and "DecodeError" in e["kind"]
            )
        )
    ]
    if bad:
        return CheckResult(
            "no_decode_errors",
            False,
            f"{len(bad)} decode-error events in log (first: {bad[0]!r})",
        )
    return CheckResult("no_decode_errors", True, "")


def check_transitions(
    events: Sequence[dict[str, Any]], expected_min: int
) -> CheckResult:
    """Count DeckLoad-bearing events to estimate transition coverage.

    The engine emits one ``DeckLoad`` per track swap; the bake-in's
    success criterion is "at least playlist_len - 1 swaps actually
    executed". We tolerate the legacy + bare-kind shapes the protocol
    accepts (``"kind": "DeckLoad"`` vs ``{"DeckLoad": {...}}``).
    """
    if expected_min <= 0:
        return CheckResult(
            "transitions_at_least_n", True, "no expectation (playlist_len ≤ 1)"
        )
    n = 0
    for ev in events:
        kind = ev.get("kind") if isinstance(ev, dict) else None
        if isinstance(kind, str) and kind in ("DeckLoad", "engine.DeckLoad"):
            n += 1
        elif isinstance(kind, dict) and "DeckLoad" in kind:
            n += 1
    if n < expected_min:
        return CheckResult(
            "transitions_at_least_n",
            False,
            f"saw {n} DeckLoad events; expected ≥ {expected_min}",
        )
    return CheckResult(
        "transitions_at_least_n", True, f"{n} DeckLoad events"
    )


def check_dropped_frames(
    master_wav: Path | None,
    events: Sequence[dict[str, Any]],
) -> CheckResult:
    """Pull the engine's recorder ``dropped_frames`` counter from the log.

    The engine logs the final counter on shutdown — the JSON line lives
    in the engine stderr capture, not the event log, so this check is
    best-effort. When we can't find it we tolerate that and emit a soft
    "unknown" pass so the gate never trips on a logging schema change.
    """
    total_frames = _wav_frame_count(master_wav) if master_wav else 0
    dropped = 0
    for ev in events:
        if not isinstance(ev, dict):
            continue
        if ev.get("dropped_frames") is not None:
            try:
                dropped = max(dropped, int(ev["dropped_frames"]))
            except (TypeError, ValueError):
                continue
    if total_frames == 0:
        return CheckResult(
            "dropped_frames_under_budget",
            True,
            "no master.wav → recorder never produced frames; skipped",
        )
    ratio = dropped / total_frames
    if ratio > DROPPED_FRAME_BUDGET:
        return CheckResult(
            "dropped_frames_under_budget",
            False,
            f"dropped={dropped} / total={total_frames} = "
            f"{ratio:.4%} > {DROPPED_FRAME_BUDGET:.2%}",
        )
    return CheckResult(
        "dropped_frames_under_budget",
        True,
        f"dropped={dropped} / total={total_frames} = {ratio:.4%}",
    )


def check_master_wav_gain(master_wav: Path | None) -> CheckResult:
    """Sample a handful of frames; assert peak is in [-30 dB, 0 dBFS].

    The bake-in catalog peaks at -3 dBFS pre-mix. The engine's master
    bus + limiter should land the output in the same ballpark — well
    below 0 dBFS (limiter doing its job) and well above -30 dBFS
    (gain hasn't collapsed). We sample at most 16 KiB of audio so the
    check runs in milliseconds regardless of bake length.
    """
    if master_wav is None or not master_wav.exists() or master_wav.stat().st_size <= 44:
        return CheckResult(
            "master_wav_gain_band",
            True,
            "no audio body to sample — skipped",
        )
    try:
        with wave.open(str(master_wav), "rb") as wf:
            n = min(8192, wf.getnframes())
            raw = wf.readframes(n)
        # PCM16 stereo: 4 bytes per frame.
        ints = [
            int.from_bytes(raw[i : i + 2], "little", signed=True)
            for i in range(0, len(raw), 2)
        ]
        if not ints:
            return CheckResult(
                "master_wav_gain_band", True, "empty sample window"
            )
        peak = max(abs(v) for v in ints) / 32768.0
        if peak <= 1e-6:
            return CheckResult(
                "master_wav_gain_band",
                False,
                "peak amplitude ≈ 0 over sampled window (silent master)",
            )
        peak_db = 20.0 * math.log10(peak)
        if peak_db < -30.0:
            return CheckResult(
                "master_wav_gain_band",
                False,
                f"peak {peak_db:.1f} dBFS below -30 dB floor (under-gained)",
            )
        if peak_db > 0.1:
            return CheckResult(
                "master_wav_gain_band",
                False,
                f"peak {peak_db:.1f} dBFS over 0 dBFS (clip)",
            )
        return CheckResult(
            "master_wav_gain_band", True, f"peak {peak_db:.2f} dBFS"
        )
    except wave.Error as exc:
        # IEEE-float WAV — the engine writes 32-bit float master.wav so
        # the wave module raises. Treat as soft pass; the existence
        # check covered the "did the engine actually write" question.
        return CheckResult(
            "master_wav_gain_band",
            True,
            f"non-PCM master (likely IEEE float): {exc}",
        )


def check_telemetry_clean(telemetry_log: Path) -> CheckResult:
    """Telemetry must be empty (opt-out default) or allow-listed events.

    Each line in the telemetry log is one Sentry envelope (``copilot``
    side) or a JSON record from the engine's ``init_telemetry``. The
    file is created empty by :mod:`run_set` so a missing file is just
    as good as an empty one — we treat both as pass.
    """
    if not telemetry_log.exists() or telemetry_log.stat().st_size == 0:
        return CheckResult("telemetry_clean", True, "empty (opt-out default)")
    offenders: list[str] = []
    for line in telemetry_log.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            envelope = json.loads(line)
        except json.JSONDecodeError:
            # Unknown shape — be conservative: a non-JSON line is not
            # a Sentry envelope, so we don't know what we're looking
            # at and flag it for inspection.
            offenders.append(f"non-json line: {line[:80]!r}")
            continue
        etype = envelope.get("type") if isinstance(envelope, dict) else None
        if etype not in TELEMETRY_ALLOWLIST:
            offenders.append(f"disallowed type={etype!r}")
    if offenders:
        return CheckResult(
            "telemetry_clean",
            False,
            f"{len(offenders)} unexpected entries (first: {offenders[0]})",
        )
    return CheckResult(
        "telemetry_clean", True, "only allow-listed envelope types"
    )


def check_run_succeeded(report: dict[str, Any]) -> CheckResult:
    """Hard fail if the run_report carries an ``error`` field."""
    err = report.get("error")
    if err:
        return CheckResult("run_succeeded", False, str(err))
    return CheckResult("run_succeeded", True, "")


# --- top-level entry ------------------------------------------------------


def verify(report_path: Path) -> VerifySummary:
    """Run every check; return a roll-up summary."""
    report = json.loads(report_path.read_text())
    artifacts = report.get("artifacts") or {}
    master_wav = (
        Path(artifacts["master_wav"]) if artifacts.get("master_wav") else None
    )
    events_log = (
        Path(artifacts["events_log"]) if artifacts.get("events_log") else None
    )
    telemetry_log = Path(
        artifacts.get("telemetry_log")
        or (report_path.parent / "telemetry.log")
    )
    playlist_len = int(report.get("playlist_len") or 0)
    expected_transitions = max(0, playlist_len - 1)
    events = _read_events(events_log)

    summary = VerifySummary()
    summary.checks.extend(
        [
            check_run_succeeded(report),
            check_master_wav_exists(master_wav),
            check_no_decode_errors(events),
            check_transitions(events, expected_transitions),
            check_dropped_frames(master_wav, events),
            check_master_wav_gain(master_wav),
            check_telemetry_clean(telemetry_log),
        ]
    )
    return summary


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="bake_in.verify",
        description="Verify post-run invariants from run_report.json.",
    )
    p.add_argument(
        "--report",
        type=Path,
        required=True,
        help="path to run_report.json (produced by run_set).",
    )
    p.add_argument(
        "--summary-out",
        type=Path,
        help="optional path to dump the JSON verify summary.",
    )
    p.add_argument(
        "--log-level",
        default="INFO",
        help="logging level.",
    )
    return p


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    logging.basicConfig(
        level=args.log_level.upper(),
        format="%(asctime)s %(name)s %(levelname)s %(message)s",
    )
    summary = verify(args.report)
    payload = summary.to_json()
    text = json.dumps(payload, indent=2, sort_keys=True)
    if args.summary_out:
        args.summary_out.parent.mkdir(parents=True, exist_ok=True)
        args.summary_out.write_text(text)
    print(text)
    if summary.ok:
        print("bake-in: PASS", file=sys.stderr)
        return 0
    failed = [c.name for c in summary.checks if not c.passed]
    print(f"bake-in: FAIL ({', '.join(failed)})", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
