"""Drive the bake-in: boot engine + copilot, enqueue tracks, sleep, capture.

This script is the "what would an operator do" layer on top of the
synth catalog. It:

1. Spawns the engine (release binary) and the copilot service. Both
   write into a per-run session directory (``HYPEHOUSE_EVENT_LOG_DIR``
   override) so the artifacts are easy to bundle.
2. Polls each process's RPC endpoint until ready (engine WS + copilot
   HTTP ``/health``).
3. Walks the manifest written by :mod:`generate_tracks` and calls
   ``library.add_track_from_directory`` so the copilot's library row +
   analyzer pass covers every track.
4. Calls ``playlist.enqueue`` for a random subset of the catalog so the
   auto-mix path has work to do.
5. Enables auto-mix on deck A and engages the co-pilot on deck B
   (``copilot.set_auto_mix`` via HTTP + ``engine.submit_event`` with
   ``CopilotEngage`` over WS).
6. Sleeps for the requested wall-clock window, then sends SIGTERM to
   both processes and waits for graceful shutdown.
7. Locates the resulting ``master.wav`` + ``events.jsonl`` + telemetry
   log and writes a per-run ``run_report.json`` the verify step reads.

Engine/copilot internals are NOT imported. All traffic flows through
the documented JSON-RPC surfaces — same as a production operator.
"""
from __future__ import annotations

import argparse
import json
import logging
import os
import random
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence

log = logging.getLogger("bake_in.run_set")

# --- env + defaults -------------------------------------------------------

#: Engine WebSocket address. Mirrors the protocol doc default and works
#: with a single in-process listener.
DEFAULT_ENGINE_WS_PORT = 8765

#: Copilot HTTP-RPC port. Matches ``copilot.http_server.DEFAULT_PORT``.
DEFAULT_COPILOT_HTTP_PORT = 8766

#: Default playlist length. 30 enqueues → 29 transitions on a 25-min bake
#: ≈ one transition per 50 s of wall clock, well inside the engine's
#: auto-mix cadence (proposer + 16-bar blend default).
DEFAULT_PLAYLIST_LEN = 30

#: Bake duration default (minutes). Sanity-run window; the ``--duration-min``
#: flag bumps it to 240 for the full v0.2 soak.
DEFAULT_DURATION_MIN = 25.0

#: Max wall-clock we will wait for a subprocess RPC endpoint to come
#: online before we declare boot failure. The engine pays a one-time
#: cpal probe cost on cold boot but it's well under 30 s on every
#: target host we run.
BOOT_TIMEOUT_S = 45.0

#: How long to wait for a graceful SIGTERM before SIGKILL. Engine should
#: shut down inside the audio-thread join + recorder finalize budget
#: (~1 s typical, 5 s pathological).
SHUTDOWN_GRACE_S = 10.0


@dataclass
class RunPaths:
    """Filesystem layout for one bake-in run."""

    root: Path
    tracks_dir: Path
    manifest: Path
    session_root: Path
    library_db: Path
    engine_stdout: Path
    engine_stderr: Path
    copilot_stdout: Path
    copilot_stderr: Path
    telemetry_log: Path
    report: Path

    @classmethod
    def under(cls, root: Path, manifest: Path) -> "RunPaths":
        root.mkdir(parents=True, exist_ok=True)
        tracks_dir = manifest.parent / "tracks"
        session_root = root / "sessions"
        session_root.mkdir(parents=True, exist_ok=True)
        return cls(
            root=root,
            tracks_dir=tracks_dir,
            manifest=manifest,
            session_root=session_root,
            library_db=root / "library.db",
            engine_stdout=root / "engine.stdout.log",
            engine_stderr=root / "engine.stderr.log",
            copilot_stdout=root / "copilot.stdout.log",
            copilot_stderr=root / "copilot.stderr.log",
            telemetry_log=root / "telemetry.log",
            report=root / "run_report.json",
        )


# --- subprocess helpers ---------------------------------------------------


def _free_port_fallback(default: int) -> int:
    """Return ``default`` if free; otherwise an OS-assigned port.

    Bake-in CI runners occasionally collide on the canonical 8765/8766
    ports with leftover dev processes; falling back keeps the harness
    re-runnable without manual port juggling.
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        try:
            sock.bind(("127.0.0.1", default))
            return default
        except OSError:
            pass
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _wait_for_port(host: str, port: int, timeout_s: float) -> bool:
    """Poll a TCP port until it accepts a connection.

    Returns ``True`` once the port responds, ``False`` if the timeout
    elapsed. We use TCP-accept rather than parsing HTTP/WS because both
    the engine and copilot open their listeners before they wire the
    handler, and that's good enough for "ready to take traffic".
    """
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=1.0):
                return True
        except OSError:
            time.sleep(0.25)
    return False


def _http_get_health(port: int) -> bool:
    """Probe the copilot's ``/health`` endpoint; return True on 200."""
    url = f"http://127.0.0.1:{port}/health"
    try:
        with urllib.request.urlopen(url, timeout=2.0) as resp:  # noqa: S310
            return 200 <= int(resp.status) < 300
    except (urllib.error.URLError, OSError):
        return False


def _http_rpc(port: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
    """Call one HTTP JSON-RPC method against the copilot.

    Wraps ``urllib`` so the harness doesn't drag aiohttp / requests into
    its dep surface. JSON-RPC envelope is built by hand — the copilot's
    HTTP server is documented in ``copilot/http_server.py`` and only
    accepts a single-request body.
    """
    url = f"http://127.0.0.1:{port}/rpc"
    payload = json.dumps(
        {"jsonrpc": "2.0", "id": int(time.time() * 1000), "method": method, "params": params}
    ).encode()
    req = urllib.request.Request(
        url, data=payload, headers={"content-type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=120.0) as resp:  # noqa: S310
        body = json.loads(resp.read().decode())
    if "error" in body and body["error"] is not None:
        raise RuntimeError(f"copilot RPC {method} failed: {body['error']}")
    result: dict[str, Any] = dict(body.get("result") or {})
    return result


def _ws_submit_event(ws_port: int, kind: dict[str, Any]) -> None:
    """Send one ``engine.submit_event`` over the engine WS bridge.

    Imported lazily so the unit-test smoke path (which never touches the
    real bridge) doesn't pay the websockets dep cost. websockets is
    already a copilot dep so it's available in every harness env.
    """
    import asyncio

    from websockets.asyncio.client import connect

    async def _send() -> None:
        url = f"ws://127.0.0.1:{ws_port}/"
        async with connect(url, max_size=None) as ws:
            envelope = {
                "jsonrpc": "2.0",
                "id": int(time.time() * 1000),
                "method": "engine.submit_event",
                "params": {"kind": kind, "source": "Copilot"},
            }
            await ws.send(json.dumps(envelope))
            reply = await ws.recv()
            decoded = json.loads(reply)
            if decoded.get("error"):
                raise RuntimeError(
                    f"engine.submit_event {kind} failed: {decoded['error']}"
                )

    asyncio.run(_send())


# --- process orchestration ------------------------------------------------


def _engine_binary(repo_root: Path) -> Path:
    """Locate the release engine binary.

    The Makefile target builds the engine with ``cargo build --release``
    in ``engine/`` so the binary lands under
    ``engine/target/release/hypehouse-engine``. Honours
    ``HYPEHOUSE_ENGINE_BIN`` for callers running the harness from a
    one-off build location.
    """
    override = os.environ.get("HYPEHOUSE_ENGINE_BIN", "").strip()
    if override:
        candidate = Path(override)
        if not candidate.exists():
            raise FileNotFoundError(
                f"HYPEHOUSE_ENGINE_BIN points at non-existent file: {candidate}"
            )
        return candidate
    cargo_target = (
        repo_root / "engine" / "target" / "release" / "hypehouse-engine"
    )
    if not cargo_target.exists():
        raise FileNotFoundError(
            f"engine release binary not found at {cargo_target} — "
            "run `make build-engine` first."
        )
    return cargo_target


def _copilot_command() -> list[str]:
    """Command vector for the copilot service.

    Honours ``HYPEHOUSE_COPILOT_CMD`` so a developer can swap in a
    locally-installed entry point (``hypehouse-copilot``) or a uv-run
    wrapper. Default uses ``python -m copilot`` which works as long as
    the copilot package is importable in the harness env.
    """
    override = os.environ.get("HYPEHOUSE_COPILOT_CMD", "").strip()
    if override:
        return override.split()
    return [sys.executable, "-m", "copilot"]


def _spawn(
    cmd: Sequence[str],
    *,
    env: dict[str, str],
    cwd: Path,
    stdout: Path,
    stderr: Path,
) -> subprocess.Popen[bytes]:
    """Spawn one subprocess with logfiles for both streams.

    ``preexec_fn=os.setsid`` puts the child in a new process group so
    SIGTERM cleanly reaches any helper threads the engine forks (audio,
    midi). Skipped on Windows where the API is unavailable; the bake-in
    workflow only runs ubuntu-latest so the unix path is the hot one.
    """
    stdout.parent.mkdir(parents=True, exist_ok=True)
    out = stdout.open("wb")
    err = stderr.open("wb")
    return subprocess.Popen(  # noqa: S603
        list(cmd),
        cwd=str(cwd),
        env=env,
        stdout=out,
        stderr=err,
        start_new_session=hasattr(os, "setsid"),
    )


def _terminate(proc: subprocess.Popen[bytes] | None, label: str) -> int | None:
    """SIGTERM with grace fallback to SIGKILL. Returns exit code or None."""
    if proc is None:
        return None
    if proc.poll() is not None:
        return proc.returncode
    log.info("sending SIGTERM to %s (pid=%s)", label, proc.pid)
    try:
        if hasattr(os, "killpg"):
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        else:
            proc.terminate()
    except ProcessLookupError:
        return proc.returncode
    try:
        return proc.wait(timeout=SHUTDOWN_GRACE_S)
    except subprocess.TimeoutExpired:
        log.warning("%s did not exit on SIGTERM — escalating to SIGKILL", label)
        try:
            if hasattr(os, "killpg"):
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            else:
                proc.kill()
        except ProcessLookupError:
            pass
        return proc.wait(timeout=5.0)


# --- public entry point ---------------------------------------------------


def _load_manifest(manifest: Path) -> dict[str, Any]:
    if not manifest.exists():
        raise FileNotFoundError(f"manifest not found: {manifest}")
    parsed: dict[str, Any] = json.loads(manifest.read_text())
    return parsed


def _find_session_dir(session_root: Path) -> Path | None:
    """Return the most-recent session subdirectory under ``session_root``.

    The engine names directories after a ULID-ish session id (see
    ``persistence::sessions``) so newest-by-mtime is the safe pick when
    a single bake-in run produces exactly one session. We log a warning
    if zero or multiple are present so the verify step has actionable
    context.
    """
    if not session_root.exists():
        return None
    candidates = [p for p in session_root.iterdir() if p.is_dir()]
    if not candidates:
        return None
    candidates.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    if len(candidates) > 1:
        log.warning(
            "multiple session dirs under %s — picking most-recent (%s). "
            "verify step will only inspect this one.",
            session_root,
            candidates[0].name,
        )
    return candidates[0]


def run(
    out_dir: Path,
    manifest: Path,
    duration_min: float = DEFAULT_DURATION_MIN,
    playlist_len: int = DEFAULT_PLAYLIST_LEN,
    *,
    engine_bin: Path | None = None,
    repo_root: Path | None = None,
    seed: int = 0xB00B,
) -> Path:
    """Execute one bake-in run; return the path to ``run_report.json``."""
    paths = RunPaths.under(out_dir, manifest)
    manifest_data = _load_manifest(manifest)
    track_specs = manifest_data["tracks"]
    if not track_specs:
        raise ValueError(f"manifest {manifest} has zero tracks")

    repo_root = repo_root or _detect_repo_root()
    engine_bin = engine_bin or _engine_binary(repo_root)
    ws_port = _free_port_fallback(DEFAULT_ENGINE_WS_PORT)
    http_port = _free_port_fallback(DEFAULT_COPILOT_HTTP_PORT)

    # Both subprocesses inherit a sanitized env where:
    #   - HYPEHOUSE_EVENT_LOG_DIR pins persistence under our session_root
    #   - HYPEHOUSE_BRIDGE_TOKEN is unset → loopback-only, no auth gate
    #   - sentry telemetry stays off (telemetry log will be empty on success)
    base_env = {k: v for k, v in os.environ.items() if not k.startswith("HYPEHOUSE_")}
    base_env.setdefault("RUST_LOG", "info")
    engine_env: dict[str, str] = {
        **base_env,
        "HYPEHOUSE_EVENT_LOG_DIR": str(paths.session_root),
        "HYPEHOUSE_WS_BIND_ADDR": f"127.0.0.1:{ws_port}",
    }
    copilot_env: dict[str, str] = {
        **base_env,
        "HYPEHOUSE_ENGINE_WS": f"ws://127.0.0.1:{ws_port}",
        "HYPEHOUSE_LIBRARY_DB": str(paths.library_db),
        "HYPEHOUSE_COPILOT_HTTP_PORT": str(http_port),
        "HYPEHOUSE_COPILOT_LOG_LEVEL": os.environ.get(
            "HYPEHOUSE_COPILOT_LOG_LEVEL", "INFO"
        ),
    }

    log.info(
        "bake-in starting: duration=%.1f min, playlist=%d, ws=:%d, http=:%d",
        duration_min,
        playlist_len,
        ws_port,
        http_port,
    )

    engine_proc: subprocess.Popen[bytes] | None = None
    copilot_proc: subprocess.Popen[bytes] | None = None
    started = time.monotonic()
    rpc_status: dict[str, Any] = {
        "library_added": 0,
        "playlist_enqueued": 0,
        "auto_mix_set": False,
        "copilot_engaged": False,
    }
    error_message: str | None = None
    try:
        engine_proc = _spawn(
            [str(engine_bin)],
            env=engine_env,
            cwd=repo_root,
            stdout=paths.engine_stdout,
            stderr=paths.engine_stderr,
        )
        if not _wait_for_port("127.0.0.1", ws_port, BOOT_TIMEOUT_S):
            raise RuntimeError(
                f"engine WS port {ws_port} never came up — see {paths.engine_stderr}"
            )
        log.info("engine WS up on :%d", ws_port)

        copilot_proc = _spawn(
            _copilot_command(),
            env=copilot_env,
            cwd=repo_root,
            stdout=paths.copilot_stdout,
            stderr=paths.copilot_stderr,
        )
        if not _wait_for_port("127.0.0.1", http_port, BOOT_TIMEOUT_S):
            raise RuntimeError(
                f"copilot HTTP port {http_port} never came up — see {paths.copilot_stderr}"
            )
        # Belt-and-braces: also wait for /health to flip green.
        health_deadline = time.monotonic() + 10.0
        while time.monotonic() < health_deadline:
            if _http_get_health(http_port):
                break
            time.sleep(0.25)
        else:
            raise RuntimeError(
                f"copilot HTTP /health never returned 200 — see {paths.copilot_stderr}"
            )
        log.info("copilot HTTP up on :%d", http_port)

        # 1. Seed the library.
        result = _http_rpc(
            http_port,
            "library.add_track_from_directory",
            {"path": str(paths.tracks_dir)},
        )
        rpc_status["library_added"] = int(result.get("added_count", 0))
        log.info(
            "library.add_track_from_directory added %d / %d tracks",
            rpc_status["library_added"],
            len(track_specs),
        )

        # 2. Enqueue playlist_len random tracks (no replacement so the
        #    auto-mix has unique downstream picks).
        rng = random.Random(seed)
        n_enqueue = min(playlist_len, len(track_specs))
        picks = rng.sample(track_specs, n_enqueue)
        for pick in picks:
            _http_rpc(http_port, "playlist.enqueue", {"track_id": pick["id"]})
        rpc_status["playlist_enqueued"] = n_enqueue
        log.info("enqueued %d tracks", n_enqueue)

        # 3. Enable auto-mix on deck A.
        _http_rpc(http_port, "copilot.set_auto_mix", {"deck": "A", "enabled": True})
        rpc_status["auto_mix_set"] = True

        # 4. Engage co-pilot on deck B via the engine bridge.
        _ws_submit_event(ws_port, {"CopilotEngage": {"deck": "B"}})
        rpc_status["copilot_engaged"] = True
        log.info("auto-mix on deck A; copilot engaged on deck B")

        # 5. Sleep for the bake window, polling subprocesses for early
        #    death so we don't snooze through a panic.
        sleep_until = time.monotonic() + duration_min * 60.0
        while time.monotonic() < sleep_until:
            time.sleep(1.0)
            if engine_proc.poll() is not None:
                raise RuntimeError(
                    f"engine exited mid-bake (rc={engine_proc.returncode}); "
                    f"see {paths.engine_stderr}"
                )
            if copilot_proc.poll() is not None:
                raise RuntimeError(
                    f"copilot exited mid-bake (rc={copilot_proc.returncode}); "
                    f"see {paths.copilot_stderr}"
                )

    except Exception as exc:  # noqa: BLE001 — captured into report
        error_message = str(exc)
        log.exception("bake-in run failed: %s", exc)
    finally:
        copilot_rc = _terminate(copilot_proc, "copilot")
        engine_rc = _terminate(engine_proc, "engine")
        elapsed_s = time.monotonic() - started

        # Try to capture the engine's session directory + master.wav
        # path. Failure to find them is signal for the verify step.
        session_dir = _find_session_dir(paths.session_root)
        master_wav = session_dir / "master.wav" if session_dir else None
        events_log = session_dir / "events.jsonl" if session_dir else None

        # Telemetry log is empty unless the operator opted in; we still
        # write the placeholder so the verify step can stat it.
        if not paths.telemetry_log.exists():
            paths.telemetry_log.write_text("")

        report = {
            "schema": 1,
            "elapsed_s": float(elapsed_s),
            "duration_min_requested": float(duration_min),
            "playlist_len": int(playlist_len),
            "rpc": rpc_status,
            "engine": {
                "binary": str(engine_bin),
                "returncode": engine_rc,
                "ws_port": ws_port,
                "stdout": str(paths.engine_stdout),
                "stderr": str(paths.engine_stderr),
            },
            "copilot": {
                "command": _copilot_command(),
                "returncode": copilot_rc,
                "http_port": http_port,
                "stdout": str(paths.copilot_stdout),
                "stderr": str(paths.copilot_stderr),
            },
            "artifacts": {
                "session_dir": str(session_dir) if session_dir else None,
                "master_wav": str(master_wav) if master_wav else None,
                "events_log": str(events_log) if events_log else None,
                "telemetry_log": str(paths.telemetry_log),
                "manifest": str(paths.manifest),
            },
            "error": error_message,
        }
        paths.report.write_text(json.dumps(report, indent=2, sort_keys=True))
        log.info("wrote run report → %s (elapsed %.1f s)", paths.report, elapsed_s)
    return paths.report


def _detect_repo_root() -> Path:
    """Walk upward from this file until we hit a checkout root."""
    here = Path(__file__).resolve()
    for parent in (here, *here.parents):
        if (parent / "engine").is_dir() and (parent / "copilot").is_dir():
            return parent
    raise RuntimeError(
        "could not locate hypehouse-live repo root (no engine/+copilot/ found)"
    )


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="bake_in.run_set",
        description="Drive the bake-in: engine + copilot + auto-mix.",
    )
    p.add_argument(
        "--out-dir",
        type=Path,
        required=True,
        help="output directory for session/, logs, run_report.json.",
    )
    p.add_argument(
        "--manifest",
        type=Path,
        required=True,
        help="manifest.json produced by generate_tracks.",
    )
    p.add_argument(
        "--duration-min",
        type=float,
        default=DEFAULT_DURATION_MIN,
        help=f"bake wall-clock minutes (default {DEFAULT_DURATION_MIN:g}).",
    )
    p.add_argument(
        "--playlist-len",
        type=int,
        default=DEFAULT_PLAYLIST_LEN,
        help=f"playlist enqueue count (default {DEFAULT_PLAYLIST_LEN}).",
    )
    p.add_argument(
        "--engine-bin",
        type=Path,
        help="override the engine binary path (else engine/target/release/...).",
    )
    p.add_argument(
        "--seed",
        type=int,
        default=0xB00B,
        help="RNG seed for playlist selection.",
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
    report = run(
        out_dir=args.out_dir,
        manifest=args.manifest,
        duration_min=args.duration_min,
        playlist_len=args.playlist_len,
        engine_bin=args.engine_bin,
        seed=args.seed,
    )
    payload = json.loads(report.read_text())
    return 1 if payload.get("error") else 0


def smoke_main(out_dir: Path) -> Path:
    """Headless smoke entry point used by the unit test.

    Writes a minimal run_report.json without spawning the engine /
    copilot. Used to verify CLI plumbing parses + serialises correctly
    on platforms where the binaries are unavailable (CI without the
    release artifact, dev workstations pre-build).
    """
    tmp_manifest = out_dir / "manifest.json"
    if not tmp_manifest.exists():
        tmp_manifest.write_text(
            json.dumps(
                {
                    "schema": 1,
                    "sample_rate": 44100,
                    "channels": 2,
                    "track_duration_s": 1.0,
                    "total_duration_s": 1.0,
                    "tracks": [
                        {
                            "id": "smoke-001",
                            "path": str(out_dir / "smoke-001.wav"),
                            "bpm": 120,
                            "camelot_key": "8A",
                        }
                    ],
                }
            )
        )
    paths = RunPaths.under(out_dir, tmp_manifest)
    report = {
        "schema": 1,
        "elapsed_s": 0.0,
        "duration_min_requested": 0.0,
        "playlist_len": 0,
        "rpc": {
            "library_added": 0,
            "playlist_enqueued": 0,
            "auto_mix_set": False,
            "copilot_engaged": False,
        },
        "engine": {"returncode": None},
        "copilot": {"returncode": None},
        "artifacts": {
            "session_dir": None,
            "master_wav": None,
            "events_log": None,
            "telemetry_log": str(paths.telemetry_log),
            "manifest": str(paths.manifest),
        },
        "error": "smoke-mode: subprocesses not spawned",
    }
    paths.telemetry_log.parent.mkdir(parents=True, exist_ok=True)
    paths.telemetry_log.write_text("")
    paths.report.write_text(json.dumps(report, indent=2, sort_keys=True))
    return paths.report


if __name__ == "__main__":
    sys.exit(main())
