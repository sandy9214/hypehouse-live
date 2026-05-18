"""``python -m copilot`` entry point.

Boots the JSON-RPC client, opens the library, and runs the decision loop
until SIGTERM/SIGINT.

Env vars:
  * ``HYPEHOUSE_ENGINE_WS`` — engine WebSocket URL. Default ``ws://127.0.0.1:8765``.
  * ``HYPEHOUSE_LIBRARY_DB`` — SQLite path. Default ``~/.hypehouse-live/library.db``.
  * ``HYPEHOUSE_COPILOT_LOG_LEVEL`` — logging level. Default ``INFO``.
"""
from __future__ import annotations

import argparse
import asyncio
import logging
import os
import signal
import sys
from pathlib import Path

from .library import TrackLibrary
from .service import CoPilotService
from .telemetry import init_telemetry


def _parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="hypehouse-copilot",
        description="HypeHouse Live AI co-pilot service",
    )
    p.add_argument(
        "--engine-ws",
        "--engine-url",
        dest="engine_ws",
        default=os.environ.get("HYPEHOUSE_ENGINE_WS"),
        help="engine WebSocket URL (overrides $HYPEHOUSE_ENGINE_WS). "
        "Default: ws://127.0.0.1:8765.",
    )
    p.add_argument(
        "--bridge-token",
        default=os.environ.get("HYPEHOUSE_BRIDGE_TOKEN", ""),
        help="bearer token for in-band auth.hello (default: $HYPEHOUSE_BRIDGE_TOKEN).",
    )
    p.add_argument(
        "--legacy-loop",
        action="store_true",
        help=(
            "use the legacy aiohttp-based service loop instead of the "
            "EngineClient + TransitionProposer wiring. "
            "Default is the new loop (PR copilot-engine-ws-subscribe)."
        ),
    )
    # HTTP RPC server — default ON. The engine bridge proxy (PR #53)
    # forwards library.* calls to http://127.0.0.1:8766/rpc, so the
    # listener must exist for the UI's library panel to work.
    http_group = p.add_mutually_exclusive_group()
    http_group.add_argument(
        "--http-server",
        action="store_true",
        default=True,
        dest="http_server",
        help=(
            "expose the JSON-RPC HTTP endpoint at http://127.0.0.1:8766/rpc "
            "(default; override port via $HYPEHOUSE_COPILOT_HTTP_PORT)."
        ),
    )
    http_group.add_argument(
        "--no-http-server",
        action="store_false",
        dest="http_server",
        help=(
            "disable the HTTP RPC endpoint; only the engine WS subscriber "
            "runs. Use for pure WS-subscriber mode (no library.* proxy)."
        ),
    )
    p.add_argument(
        "--library-db",
        default=os.environ.get(
            "HYPEHOUSE_LIBRARY_DB", str(Path("~/.hypehouse-live/library.db"))
        ),
        help="SQLite library path (default ~/.hypehouse-live/library.db).",
    )
    p.add_argument(
        "--log-level",
        default=os.environ.get("HYPEHOUSE_COPILOT_LOG_LEVEL", "INFO"),
        help="logging level (DEBUG/INFO/WARNING/ERROR).",
    )
    return p.parse_args(argv)


async def _run(args: argparse.Namespace) -> int:
    logging.basicConfig(
        level=args.log_level.upper(),
        format="%(asctime)s %(name)s %(levelname)s %(message)s",
    )
    log = logging.getLogger("copilot.main")
    # Opt-in telemetry. Always called; the module returns False (and
    # logs the reason) when the operator has not enabled it. Never
    # raises — a missing sentry-sdk install is treated as "stay off".
    init_telemetry()
    log.info("opening library at %s", args.library_db)
    library = TrackLibrary(args.library_db)
    service = CoPilotService(
        library,
        engine_ws_url=args.engine_ws,
        bridge_token=args.bridge_token,
    )
    if args.http_server:
        run_coro = service.run_with_http_server(
            use_legacy_engine_loop=args.legacy_loop,
        )
    else:
        run_coro = (
            service.run() if args.legacy_loop else service.run_with_proposer()
        )

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()

    def _request_stop(*_: object) -> None:  # noqa: ANN401
        log.info("received shutdown signal")
        stop.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, _request_stop)
        except NotImplementedError:
            # Windows / non-mainloop — fall back to default handlers.
            signal.signal(sig, _request_stop)

    runner = asyncio.create_task(run_coro, name="copilot-service-run")
    waiter = asyncio.create_task(stop.wait(), name="copilot-stop-waiter")
    try:
        done, _pending = await asyncio.wait(
            {runner, waiter}, return_when=asyncio.FIRST_COMPLETED
        )
        # Stop requested — cancel the runner cleanly.
        runner.cancel()
        try:
            await runner
        except (asyncio.CancelledError, Exception):  # noqa: BLE001
            pass
        for d in done:
            exc = d.exception() if d.done() and not d.cancelled() else None
            if exc is not None and d is runner:
                log.error("service exited with %s", exc, exc_info=exc)
                return 1
        return 0
    finally:
        library.close()


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv if argv is not None else sys.argv[1:])
    try:
        return asyncio.run(_run(args))
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
