"""HypeHouse Live AI co-pilot service.

The co-pilot is a Python 3.11 long-running process that speaks JSON-RPC over a
WebSocket to the Rust audio engine. When the user toggles co-pilot mode on a
deck, the service:

* subscribes to engine state-change notifications,
* picks the next track from a local library when the playing deck nears its
  end (ADR-002 — <30s remaining), and
* emits an event sequence (LoopArm, CopilotEngage, Crossfader ramp) the
  engine can fold into its event-sourced state (ADR-003).

Pure decision functions live in :mod:`copilot.decisions`; the network loop is
in :mod:`copilot.service`; the entry point is :mod:`copilot.main`.

The analyzer + mashup primitives are vendored from HypeHouse v1 — see
``copilot/vendor/VENDOR.md`` for the source manifest.
"""
from __future__ import annotations

__version__ = "0.1.0"
