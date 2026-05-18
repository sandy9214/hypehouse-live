"""Waveform peak-pair tests.

Covers five concerns:

1. ``compute_peaks`` (via the internal ``_peaks_from_samples`` packer)
   produces a packed byte buffer of the expected shape.
2. Library schema migrates v3 → v4 in place — the ``waveform_peaks``
   column is added and existing rows survive.
3. ``library.get_waveform`` returns ``None`` for missing tracks and
   for tracks with NULL peaks.
4. ``library.get_waveform`` RPC produces a base64 wire envelope when
   peaks are present.
5. ``set_waveform`` followed by ``get_waveform`` is byte-identical
   (idempotent / round-trip).

Heavy librosa-driven decode is *not* exercised here — the integration
boundary is tested via ``_peaks_from_samples`` with synthetic numpy
arrays. This keeps the test hermetic + fast (no madmom warmup).
"""
from __future__ import annotations

import base64
import sqlite3
from pathlib import Path

import pytest

from copilot.library import (
    TRACK_SCHEMA_VERSION,
    TrackLibrary,
    TrackRef,
)
from copilot.library_rpc import LibraryRpcHandler
from copilot.waveform import (
    BYTES_PER_BUCKET,
    DEFAULT_TARGET_SAMPLES,
    _peaks_from_samples,
    expected_bytes,
)

# Most RPC tests in this module are async.
_asyncio = pytest.mark.asyncio


# --------------------------------------------------------------------- #
# 1. compute_peaks shape                                                #
# --------------------------------------------------------------------- #


def test_peaks_from_samples_shape_and_range() -> None:
    """A sine-like signal compresses cleanly into target_samples buckets.

    Asserts:
      * output length == 2 * target_samples (one byte each for min/max)
      * every byte is in [-128, 127] when reinterpreted as i8
      * the min byte is <= the max byte for every bucket
      * a full-scale ±1.0 signal produces saturation (min near -128,
        max near +127) somewhere in the buffer.
    """
    import numpy as np

    target = 256
    # Build a 5-second 1 kHz sine at 22050 Hz — guaranteed to span the
    # full [-1.0, 1.0] range so saturation lands on i8 boundaries.
    sr = 22050
    t = np.linspace(0, 5.0, sr * 5, endpoint=False, dtype=np.float32)
    signal = np.sin(2 * np.pi * 1000.0 * t).astype(np.float32)

    out = _peaks_from_samples(signal, target)

    assert len(out) == target * BYTES_PER_BUCKET == expected_bytes(target)
    # Per-bucket invariant: min <= max in i8 space.
    for i in range(target):
        lo = int.from_bytes(out[i * 2 : i * 2 + 1], "big", signed=True)
        hi = int.from_bytes(out[i * 2 + 1 : i * 2 + 2], "big", signed=True)
        assert -128 <= lo <= 127
        assert -128 <= hi <= 127
        assert lo <= hi, f"bucket {i}: min {lo} > max {hi}"
    # Saturation should occur somewhere across a full-scale sine.
    mins = [
        int.from_bytes(out[i * 2 : i * 2 + 1], "big", signed=True)
        for i in range(target)
    ]
    maxes = [
        int.from_bytes(out[i * 2 + 1 : i * 2 + 2], "big", signed=True)
        for i in range(target)
    ]
    assert min(mins) <= -120
    assert max(maxes) >= 120


# --------------------------------------------------------------------- #
# 2. schema migration v3 → v4                                           #
# --------------------------------------------------------------------- #


def test_schema_migration_adds_waveform_peaks_column(tmp_path: Path) -> None:
    """A v3-shaped DB (no waveform_peaks column) opened by current
    TrackLibrary must gain the column + bump schema_version to 4."""
    db_path = tmp_path / "v3.db"
    # Hand-roll a v3 schema — tracks + all v3 columns including
    # hot_cues_json. No waveform_peaks. The schema_version table is
    # populated with 3 so we can verify it's bumped to 4.
    conn = sqlite3.connect(str(db_path))
    conn.executescript(
        """
        CREATE TABLE tracks (
            track_id            TEXT PRIMARY KEY,
            path                TEXT NOT NULL,
            bpm                 REAL NOT NULL,
            camelot_key         TEXT NOT NULL,
            energy              REAL NOT NULL,
            duration_s          REAL NOT NULL,
            beat_grid_anchor_ms INTEGER NOT NULL DEFAULT 0,
            beat_period_ms      REAL NOT NULL DEFAULT 500.0,
            downbeats_json      TEXT NOT NULL DEFAULT '[]',
            hot_cues_json       TEXT NOT NULL DEFAULT '[null,null,null,null,null,null,null,null]'
        );
        CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
        INSERT INTO schema_version VALUES (3);
        """
    )
    conn.execute(
        "INSERT INTO tracks "
        "(track_id, path, bpm, camelot_key, energy, duration_s) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        ("legacy", "/legacy.mp3", 124.0, "8B", 0.2, 210.0),
    )
    conn.commit()
    conn.close()

    lib = TrackLibrary(db_path)
    try:
        cols = {
            row[1]
            for row in lib._conn.execute("PRAGMA table_info(tracks)")
        }
        assert "waveform_peaks" in cols
        # Pre-existing row still readable.
        ref = lib.get("legacy")
        assert ref is not None and ref.bpm == 124.0
        # And it has no peaks yet (NULL backfill).
        assert lib.get_waveform("legacy") is None
        # Schema version stamp bumped.
        version = lib._conn.execute(
            "SELECT version FROM schema_version"
        ).fetchone()
        assert version["version"] == TRACK_SCHEMA_VERSION == 4
    finally:
        lib.close()


# --------------------------------------------------------------------- #
# 3. get_waveform handles missing + NULL                                #
# --------------------------------------------------------------------- #


def test_get_waveform_missing_track_returns_none(library: TrackLibrary) -> None:
    assert library.get_waveform("does-not-exist") is None


def test_get_waveform_null_returns_none(library: TrackLibrary) -> None:
    """A track inserted without peaks (the common test-fixture path)
    yields None — the lazy-compute branch in the RPC handler decides
    whether to fill it on first request."""
    library.add_track(
        TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0)
    )
    assert library.get_waveform("a") is None


# --------------------------------------------------------------------- #
# 4. RPC base64 wire shape                                              #
# --------------------------------------------------------------------- #


@_asyncio
async def test_get_waveform_rpc_returns_base64(
    library: TrackLibrary,
) -> None:
    """``library.get_waveform`` returns a wire envelope with a base64
    string when peaks are present, and the decoded bytes match what
    was stored."""
    library.add_track(
        TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0)
    )
    # Synthetic peaks — 16 buckets × 2 bytes. Easy to round-trip.
    peaks = bytes(range(32))
    library.set_waveform("a", peaks)

    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.get_waveform", {"track_id": "a"}
    )
    assert result["track_id"] == "a"
    assert isinstance(result["peaks_b64"], str)
    decoded = base64.b64decode(result["peaks_b64"])
    assert decoded == peaks


@_asyncio
async def test_get_waveform_rpc_missing_track_returns_null(
    library: TrackLibrary,
) -> None:
    """Unknown ``track_id`` returns peaks_b64: null rather than an
    error envelope — UI's single fetch path branches on null only."""
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.get_waveform", {"track_id": "phantom"}
    )
    assert result == {"track_id": "phantom", "peaks_b64": None}


# --------------------------------------------------------------------- #
# 5. set_waveform → get_waveform round-trip (idempotency)               #
# --------------------------------------------------------------------- #


def test_set_get_waveform_round_trip_is_byte_identical(
    library: TrackLibrary,
) -> None:
    library.add_track(
        TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0)
    )
    peaks_a = bytes([0, 1, 2, 3, 4, 5, 6, 7])
    library.set_waveform("a", peaks_a)
    assert library.get_waveform("a") == peaks_a

    # Overwrite — last write wins, bytes match exactly.
    peaks_b = bytes([255, 254, 253, 252, 251, 250, 249, 248])
    library.set_waveform("a", peaks_b)
    assert library.get_waveform("a") == peaks_b


def test_set_waveform_unknown_track_raises_key_error(
    library: TrackLibrary,
) -> None:
    """Unknown ``track_id`` raises KeyError so callers don't silently
    no-op into a missing waveform persistence."""
    with pytest.raises(KeyError):
        library.set_waveform("phantom", b"\x00\x01")


# --------------------------------------------------------------------- #
# Default constants — guard against accidental regressions              #
# --------------------------------------------------------------------- #


def test_default_target_samples_unchanged() -> None:
    """Wire compatibility: the UI assumes 2000 buckets by default.
    Changing this constant requires a coordinated bump on the UI
    side — guard with a test rather than rely on review catching it.
    """
    assert DEFAULT_TARGET_SAMPLES == 2000
    assert expected_bytes(DEFAULT_TARGET_SAMPLES) == 4000
