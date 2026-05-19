"""Synthetic bake-in harness for the v0.2 beta acceptance gate.

Three-step pipeline:

* :mod:`scripts.bake_in.generate_tracks` — synthesize a deterministic
  catalog of test WAVs at known BPMs + Camelot keys.
* :mod:`scripts.bake_in.run_set` — boot engine + copilot, enqueue the
  synth catalog, sleep ``--duration-min``, then shut down cleanly and
  capture the session artifacts (master.wav, events.jsonl, telemetry
  log).
* :mod:`scripts.bake_in.verify` — assert post-run invariants (no decode
  panics, dropped-frame budget, transition count, telemetry hygiene).

Defaults to a 25-minute "sanity" run so a developer can validate end-to-
end on a laptop without baby-sitting a four-hour bake. ``DURATION_MIN``
(env or CLI flag) bumps it to the full 240-minute soak the v0.2 release
checklist demands.

The harness deliberately does not import anything from ``engine/`` or
``copilot/``. All wire traffic goes through the documented JSON-RPC
surface (see ``docs/api/ws-protocol.md``), so the bake-in catches the
same regressions a real operator would hit in production.
"""

__all__ = ("generate_tracks", "run_set", "verify")
