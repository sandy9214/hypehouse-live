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


def _parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="hypehouse-copilot",
        description="HypeHouse Live AI co-pilot service",
    )
    p.add_argument(
        "--engine-ws",
        default=os.environ.get("HYPEHOUSE_ENGINE_WS"),
        help="engine WebSocket URL (overrides $HYPEHOUSE_ENGINE_WS).",
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
    log.info("opening library at %s", args.library_db)
    library = TrackLibrary(args.library_db)
    service = CoPilotService(library, engine_ws_url=args.engine_ws)

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

    runner = asyncio.create_task(service.run(), name="copilot-service-run")
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
