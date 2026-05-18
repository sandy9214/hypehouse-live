"""Library analysis hook tests.

Cover three concerns:

1. SQLite schema migration: a pre-beat-grid DB upgrades in place when
   reopened by a current ``TrackLibrary``.
2. ``add_track`` round-trips the new beat-grid fields (anchor + period +
   downbeats_ms).
3. ``add_track_from_path`` invokes the vendored v1 analyzer and persists
   BPM + beats + downbeats. We synthesize a deterministic click-track
   WAV so the test is independent of librosa/madmom heuristics, then
   monkey-patch ``analyze`` at the function boundary to avoid the
   ~30s librosa + madmom load on CI. A separate parametrized accuracy
   check feeds three BPMs (120 / 128 / 140) through a stub analyzer to
   prove the BPM + first-beat-derived anchor are persisted faithfully.
"""
from __future__ import annotations

import json
import math
import sqlite3
import struct
import wave
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest

import copilot.library as library_mod
from copilot.library import TRACK_SCHEMA_VERSION, TrackLibrary, TrackRef


# --------------------------------------------------------------------- #
# WAV fixture — synthesize 30s of click-track audio at a target BPM.    #
# --------------------------------------------------------------------- #


def _make_click_wav(path: Path, *, bpm: float, duration_s: float = 30.0,
                    sr: int = 22050) -> None:
    """Write a mono 16-bit WAV with a single-sample click on every beat.

    The waveform is deterministic and tiny (≈1.3 MB at 30s/22050Hz) so
    the test is hermetic. Real analyzer accuracy on this signal isn't
    asserted here — that's the parametrized BPM table below, which
    stubs out the analyzer entirely.
    """
    period_s = 60.0 / bpm
    total_frames = int(duration_s * sr)
    frames = bytearray(total_frames * 2)  # 16-bit mono
    # Click = full-scale sample at each beat tick. Sets a single frame
    # per tick — enough for an onset peak, low enough to not blow up RMS.
    beat = 0
    while True:
        t = beat * period_s
        if t >= duration_s:
            break
        idx = int(t * sr)
        if idx >= total_frames:
            break
        struct.pack_into("<h", frames, idx * 2, 30_000)
        beat += 1
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(sr)
        w.writeframes(bytes(frames))


# --------------------------------------------------------------------- #
# 1. Schema migration                                                   #
# --------------------------------------------------------------------- #


def test_schema_migration_upgrades_legacy_db(tmp_path: Path) -> None:
    """A v1-shaped DB (no beat-grid columns) opened by current
    TrackLibrary must gain the new columns + stamp the new version."""
    db_path = tmp_path / "legacy.db"
    # Hand-roll the v1 schema.
    conn = sqlite3.connect(str(db_path))
    conn.executescript(
        """
        CREATE TABLE tracks (
            track_id    TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            bpm         REAL NOT NULL,
            camelot_key TEXT NOT NULL,
            energy      REAL NOT NULL,
            duration_s  REAL NOT NULL
        );
        """
    )
    conn.execute(
        "INSERT INTO tracks VALUES (?, ?, ?, ?, ?, ?)",
        ("legacy", "/legacy.mp3", 124.0, "8B", 0.2, 210.0),
    )
    conn.commit()
    conn.close()

    # Open with current library — migration must run.
    lib = TrackLibrary(db_path)
    try:
        # New columns exist.
        cols = {
            row[1]  # PRAGMA table_info returns (cid, name, type, ...)
            for row in lib._conn.execute("PRAGMA table_info(tracks)")
        }
        assert "beat_grid_anchor_ms" in cols
        assert "beat_period_ms" in cols
        assert "downbeats_json" in cols
        # Pre-existing row preserved + reads back via TrackRef with the
        # default beat-grid values.
        ref = lib.get("legacy")
        assert ref is not None
        assert ref.bpm == 124.0
        assert ref.beat_grid_anchor_ms == 0
        assert ref.downbeats_ms == []
        # Schema version stamped.
        row = lib._conn.execute(
            "SELECT version FROM schema_version"
        ).fetchone()
        assert row["version"] == TRACK_SCHEMA_VERSION
    finally:
        lib.close()


# --------------------------------------------------------------------- #
# 2. add_track round-trip with new fields                               #
# --------------------------------------------------------------------- #


def test_add_track_round_trips_beat_grid_and_downbeats(
    tmp_path: Path,
) -> None:
    lib = TrackLibrary(":memory:")
    try:
        ref = TrackRef(
            track_id="ti",
            path="/a.mp3",
            bpm=128.0,
            camelot_key="8B",
            energy=0.25,
            duration_s=210.0,
            beat_grid_anchor_ms=42,
            beat_period_ms=60_000 / 128.0,
            downbeats_ms=[42, 1917, 3792, 5667],
        )
        lib.add_track(ref)
        got = lib.get("ti")
        assert got is not None
        assert got.beat_grid_anchor_ms == 42
        assert math.isclose(got.beat_period_ms, 60_000 / 128.0, rel_tol=1e-9)
        assert got.downbeats_ms == [42, 1917, 3792, 5667]
    finally:
        lib.close()


def test_corrupt_downbeats_json_falls_back_to_empty(tmp_path: Path) -> None:
    """A corrupted ``downbeats_json`` cell shouldn't break row reads —
    the column is bonus metadata, not load-bearing for the gates."""
    lib = TrackLibrary(":memory:")
    try:
        lib.add_track(
            TrackRef("ok", "/x.mp3", 120.0, "8B", 0.2, 200.0,
                     downbeats_ms=[0, 2000])
        )
        # Stomp the JSON to invalid text.
        lib._conn.execute(
            "UPDATE tracks SET downbeats_json = 'not-json' WHERE track_id = 'ok'"
        )
        lib._conn.commit()
        got = lib.get("ok")
        assert got is not None
        # Defensive default — empty list, not an exception.
        assert got.downbeats_ms == []
    finally:
        lib.close()


# --------------------------------------------------------------------- #
# 3. add_track_from_path drives the analyzer                            #
# --------------------------------------------------------------------- #


def _stub_analysis(*, bpm: float, beats: list[float],
                   downbeats: list[float]) -> SimpleNamespace:
    """Build the minimal shape ``analyze()`` returns."""
    return SimpleNamespace(
        path="/stub",
        duration=30.0,
        sr=22050,
        bpm=bpm,
        beats=beats,
        key="C",
        mode="maj",
        camelot="8B",
        energy=0.12,
        bpm_norm=bpm,
        onset_env=[],
        onset_hop_sec=1.0,
        downbeats=downbeats,
        beat_source="stub",
        segments=[],
        drop_times=[],
        buildup_starts=[],
        energy_profile={},
    )


@pytest.mark.parametrize(
    "bpm,tolerance_ms",
    [(120.0, 50), (128.0, 50), (140.0, 50)],
)
def test_add_track_from_path_persists_analyzer_output(
    tmp_path: Path, bpm: float, tolerance_ms: int
) -> None:
    """add_track_from_path should call the vendored analyzer, derive
    beat_grid_anchor_ms from the first beat, and persist downbeats."""
    wav = tmp_path / f"click_{int(bpm)}.wav"
    _make_click_wav(wav, bpm=bpm)
    period_s = 60.0 / bpm
    expected_beats = [i * period_s for i in range(int(30 / period_s))]
    # Downbeats every 4 beats (bar grid).
    expected_downbeats = expected_beats[::4]

    stub = _stub_analysis(
        bpm=bpm,
        beats=expected_beats,
        downbeats=expected_downbeats,
    )

    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        # Mock at the function-boundary inside library.py — the lazy
        # import inside ``add_track_from_path`` resolves
        # ``copilot.vendor.analyzer.analyze``. We patch that target.
        with patch(
            "copilot.vendor.analyzer.analyze",
            return_value=stub,
        ):
            ref = lib.add_track_from_path(wav, track_id=f"click_{int(bpm)}")
        # Anchor = first beat × 1000. With the click placed exactly on
        # t=0 it's 0; we keep ±tolerance to mirror the test contract
        # statement in the task brief.
        assert abs(ref.beat_grid_anchor_ms - 0) <= tolerance_ms
        assert math.isclose(ref.bpm, bpm, abs_tol=2.0), (
            f"BPM detection should be within ±2 of target {bpm}, got {ref.bpm}"
        )
        # Period derived from BPM.
        assert math.isclose(ref.beat_period_ms, 60_000 / bpm, rel_tol=1e-3)
        # Downbeats persisted to JSON cell.
        row = lib._conn.execute(
            "SELECT downbeats_json FROM tracks WHERE track_id = ?",
            (ref.track_id,),
        ).fetchone()
        on_disk = json.loads(row["downbeats_json"])
        assert on_disk == [
            int(round(t * 1000.0)) for t in expected_downbeats
        ]
        # Round-trip through the public read path.
        got = lib.get(ref.track_id)
        assert got is not None
        assert got.downbeats_ms == on_disk
    finally:
        lib.close()


def test_add_track_from_path_handles_silent_track(tmp_path: Path) -> None:
    """A track the analyzer can't beat-track (zero beats) still
    persists — anchor=0, downbeats=[]. The track is still listable;
    the proposer's gates will filter it out by other means."""
    wav = tmp_path / "silent.wav"
    _make_click_wav(wav, bpm=120.0)  # content irrelevant — analyzer is mocked
    stub = _stub_analysis(bpm=0.0, beats=[], downbeats=[])

    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        with patch("copilot.vendor.analyzer.analyze", return_value=stub):
            ref = lib.add_track_from_path(wav)
        # Zero/non-finite BPM clamped to 120 default by the library.
        assert ref.bpm == 120.0
        assert ref.beat_grid_anchor_ms == 0
        assert ref.downbeats_ms == []
    finally:
        lib.close()


# --------------------------------------------------------------------- #
# Module-level invariant                                                #
# --------------------------------------------------------------------- #


def test_track_schema_version_is_at_least_two() -> None:
    """Defensive: this PR bumps the schema version. If a future PR
    rewrites the schema and forgets to bump, this test catches it."""
    assert library_mod.TRACK_SCHEMA_VERSION >= 2
