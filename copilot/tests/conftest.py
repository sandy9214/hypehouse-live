"""Shared test fixtures.

A in-memory ``TrackLibrary`` is the most common dependency, so it's a fixture
here. Each test gets its own DB to keep ordering-independent.
"""
from __future__ import annotations

import pytest

from copilot.library import TrackLibrary, TrackRef


@pytest.fixture
def library() -> TrackLibrary:
    lib = TrackLibrary(":memory:")
    try:
        yield lib
    finally:
        lib.close()


@pytest.fixture
def populated_library(library: TrackLibrary) -> TrackLibrary:
    """A small canonical library used by multiple tests.

    Tracks are arranged so:
      * t1 (the "playing" track) is at 124 BPM / 8B / energy 0.20.
      * t2 is the obvious-best match (125 BPM / 8B / energy 0.22).
      * t3 is far in BPM (148 BPM) — should be gated out.
      * t4 is far in key (3A — Camelot distance > 2) — should be gated out.
      * t5 is a borderline runner-up (122 BPM / 9B / energy 0.16 — energy dip).
    """
    rows = [
        TrackRef("t1", "/tracks/t1.mp3", bpm=124.0, camelot_key="8B",
                 energy=0.20, duration_s=210.0),
        TrackRef("t2", "/tracks/t2.mp3", bpm=125.0, camelot_key="8B",
                 energy=0.22, duration_s=225.0),
        TrackRef("t3", "/tracks/t3.mp3", bpm=148.0, camelot_key="8B",
                 energy=0.30, duration_s=200.0),
        TrackRef("t4", "/tracks/t4.mp3", bpm=124.5, camelot_key="3A",
                 energy=0.21, duration_s=240.0),
        TrackRef("t5", "/tracks/t5.mp3", bpm=122.0, camelot_key="9B",
                 energy=0.16, duration_s=215.0),
    ]
    for r in rows:
        library.add_track(r)
    return library
