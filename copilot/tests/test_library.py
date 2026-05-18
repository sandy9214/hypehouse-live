"""Library / catalog tests.

Cover the BPM + Camelot gate logic in ``TrackLibrary.pick_compatible_for``,
plus the small util functions that back it.
"""
from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from copilot.library import (
    HOT_CUE_SLOTS,
    TRACK_SCHEMA_VERSION,
    TrackLibrary,
    TrackRef,
    bpm_stretch_ratio,
    camelot_distance,
)


# -------- camelot_distance: unit --------


@pytest.mark.parametrize(
    "a,b,expected",
    [
        ("8B", "8B", 0),       # identical
        ("8B", "9B", 1),       # adjacent number, same letter
        ("8B", "8A", 1),       # same number, different letter (relative)
        ("8B", "10B", 2),      # two steps on the wheel
        ("8B", "11B", 3),      # three steps — too far
        ("12B", "1B", 1),      # wraparound
        ("1B", "12B", 1),
        ("8B", "??", 99),      # malformed → big sentinel
        ("", "8B", 99),
    ],
)
def test_camelot_distance(a: str, b: str, expected: int):
    assert camelot_distance(a, b) == expected


def test_bpm_stretch_ratio_rejects_nonpositive():
    assert bpm_stretch_ratio(0.0, 124.0) == float("inf")
    assert bpm_stretch_ratio(124.0, -5.0) == float("inf")


def test_bpm_stretch_ratio_symmetric_relative_to_playing():
    # 4% stretch in either direction.
    assert abs(bpm_stretch_ratio(124.0, 129.0) - (5.0 / 124.0)) < 1e-9


# -------- TrackLibrary CRUD --------


def test_library_round_trip(library: TrackLibrary):
    ref = TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0)
    library.add_track(ref)
    fetched = library.get("a")
    assert fetched == ref


def test_library_replace_on_conflict(library: TrackLibrary):
    library.add_track(TrackRef("a", "/old.mp3", 120.0, "8B", 0.2, 200.0))
    library.add_track(TrackRef("a", "/new.mp3", 125.0, "9B", 0.3, 210.0))
    fetched = library.get("a")
    assert fetched is not None
    assert fetched.path == "/new.mp3"
    assert fetched.bpm == 125.0


# -------- pick_compatible_for: the gate logic --------


def _seed_five_tracks(lib: TrackLibrary) -> None:
    """Same fixture seeding as in ``populated_library``, inlined so this test
    file is self-contained and the asserts read alongside the data."""
    lib.add_track(TrackRef("playing", "/p.mp3", 124.0, "8B", 0.20, 210.0))
    lib.add_track(TrackRef("close",   "/a.mp3", 125.0, "8B", 0.22, 220.0))
    lib.add_track(TrackRef("far_bpm", "/b.mp3", 148.0, "8B", 0.30, 200.0))  # 19% stretch — out
    lib.add_track(TrackRef("far_key", "/c.mp3", 124.5, "3A", 0.21, 240.0))  # too far on wheel
    lib.add_track(TrackRef("border",  "/d.mp3", 122.0, "9B", 0.16, 215.0))  # OK, runner-up


def test_pick_compatible_filters_bpm_and_key(library: TrackLibrary):
    _seed_five_tracks(library)
    out = library.pick_compatible_for(
        playing_bpm=124.0,
        playing_camelot="8B",
        exclude_ids={"playing"},
    )
    ids = [t.track_id for t in out]
    # Both gates work:
    assert "far_bpm" not in ids
    assert "far_key" not in ids
    # And the two valid candidates are returned, with the BPM-closer + key-
    # closer one ranked first.
    assert ids[0] == "close"
    assert "border" in ids


def test_pick_compatible_respects_exclude(library: TrackLibrary):
    _seed_five_tracks(library)
    out = library.pick_compatible_for(
        playing_bpm=124.0, playing_camelot="8B", exclude_ids={"playing", "close"},
    )
    ids = [t.track_id for t in out]
    assert "close" not in ids
    assert "border" in ids


def test_pick_compatible_top_k_cap(library: TrackLibrary):
    """Top-K cap is respected even when many tracks pass the gates."""
    # Insert 10 near-identical tracks.
    for i in range(10):
        library.add_track(
            TrackRef(f"t{i}", f"/t{i}.mp3", 124.0 + 0.1 * i, "8B", 0.20, 200.0)
        )
    out = library.pick_compatible_for(
        playing_bpm=124.0, playing_camelot="8B", exclude_ids=set(), top_k=3,
    )
    assert len(out) == 3


def test_pick_compatible_empty_library(library: TrackLibrary):
    out = library.pick_compatible_for(playing_bpm=124.0, playing_camelot="8B")
    assert out == []


# -------- hot-cue persistence (PR: hot-cue persistence) --------


def test_schema_version_is_current():
    """The constant must match the migration plan documented in library.py.

    Bumped from v4 → v5 in the stem-separation scaffold PR (adds the
    ``stems_dir`` + ``stems_status`` columns). v5 → v6 in the
    preset-snapshots PR (adds the ``presets`` table — separate table,
    not a column on ``tracks``). Pinning to a literal here rather than
    a >= comparison so a future regression that *lowers* the version
    fails loudly.
    """
    assert TRACK_SCHEMA_VERSION == 6
    assert HOT_CUE_SLOTS == 8


def test_hot_cues_default_is_eight_nones(library: TrackLibrary):
    """A freshly-added track without explicit cues defaults to all-None."""
    library.add_track(TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0))
    fetched = library.get("a")
    assert fetched is not None
    assert fetched.hot_cues == [None] * HOT_CUE_SLOTS


def test_set_hot_cues_round_trip(library: TrackLibrary):
    library.add_track(TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0))
    cues: list[int | None] = [0, 1500, None, 8000, None, None, 60_000, None]
    returned = library.set_hot_cues("a", cues)
    assert returned.hot_cues == cues
    fetched = library.get("a")
    assert fetched is not None
    assert fetched.hot_cues == cues


def test_set_hot_cues_unknown_track_raises_keyerror(library: TrackLibrary):
    with pytest.raises(KeyError):
        library.set_hot_cues("does-not-exist", [None] * HOT_CUE_SLOTS)


@pytest.mark.parametrize(
    "bad",
    [
        [None, None, None],  # wrong length (3)
        [None] * 9,  # wrong length (9)
        [None, None, None, None, None, None, None, "100"],  # str slot
        [None, None, None, None, None, None, None, -1],  # negative
        [None, None, None, None, None, None, None, True],  # bool subclass
    ],
)
def test_set_hot_cues_rejects_bad_shapes(library: TrackLibrary, bad):
    library.add_track(TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0))
    with pytest.raises(ValueError):
        library.set_hot_cues("a", bad)


def test_migration_v2_to_v3_backfills_hot_cues_column(tmp_path: Path):
    """Existing v2 DBs (no ``hot_cues_json`` column) must auto-migrate."""
    db_file = tmp_path / "v2.db"
    # Hand-build a v2-shaped DB so the migration path is exercised end-
    # to-end. Mirrors exactly the schema that landed in PR #30 + #40.
    conn = sqlite3.connect(str(db_file))
    conn.executescript(
        """
        CREATE TABLE tracks (
            track_id    TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            bpm         REAL NOT NULL,
            camelot_key TEXT NOT NULL,
            energy      REAL NOT NULL,
            duration_s  REAL NOT NULL,
            beat_grid_anchor_ms INTEGER NOT NULL DEFAULT 0,
            beat_period_ms REAL NOT NULL DEFAULT 500.0,
            downbeats_json TEXT NOT NULL DEFAULT '[]'
        );
        CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
        INSERT INTO schema_version (version) VALUES (2);
        INSERT INTO tracks
            (track_id, path, bpm, camelot_key, energy, duration_s)
            VALUES ('legacy', '/legacy.mp3', 120.0, '8B', 0.2, 200.0);
        """
    )
    conn.commit()
    conn.close()

    # Opening with the new code path must add the column + bump the
    # stamp. The legacy row reads back with the default empty-cues
    # array (matching the DDL DEFAULT literal).
    lib = TrackLibrary(db_file)
    try:
        cols = {
            row["name"]
            for row in lib._conn.execute("PRAGMA table_info(tracks)")
        }
        assert "hot_cues_json" in cols
        version_row = lib._conn.execute(
            "SELECT version FROM schema_version"
        ).fetchone()
        assert version_row["version"] == TRACK_SCHEMA_VERSION
        legacy = lib.get("legacy")
        assert legacy is not None
        assert legacy.hot_cues == [None] * HOT_CUE_SLOTS
        # And new writes round-trip through the migrated table.
        lib.set_hot_cues("legacy", [42] + [None] * 7)
        re_read = lib.get("legacy")
        assert re_read is not None
        assert re_read.hot_cues[0] == 42
    finally:
        lib.close()


def test_corrupted_hot_cues_json_falls_back_to_empty(library: TrackLibrary):
    """A garbage ``hot_cues_json`` cell shouldn't break row reads."""
    library.add_track(TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0))
    # Overwrite the cell with invalid JSON behind the library's back.
    library._conn.execute(
        "UPDATE tracks SET hot_cues_json = ? WHERE track_id = ?",
        ("{not json}", "a"),
    )
    library._conn.commit()
    fetched = library.get("a")
    assert fetched is not None
    assert fetched.hot_cues == [None] * HOT_CUE_SLOTS


def test_hot_cues_json_padded_when_short(library: TrackLibrary):
    """A short JSON array (older slot count) pads to 8 with None."""
    library.add_track(TrackRef("a", "/a.mp3", 120.0, "8B", 0.2, 200.0))
    library._conn.execute(
        "UPDATE tracks SET hot_cues_json = ? WHERE track_id = ?",
        (json.dumps([100, 200, 300]), "a"),
    )
    library._conn.commit()
    fetched = library.get("a")
    assert fetched is not None
    assert fetched.hot_cues == [100, 200, 300, None, None, None, None, None]


def test_pick_compatible_widening_stretch_unlocks_more(library: TrackLibrary):
    """Loosening the BPM window must let previously-rejected tracks through."""
    _seed_five_tracks(library)
    # With the relaxed window, the 148-BPM "far_bpm" track passes.
    out = library.pick_compatible_for(
        playing_bpm=124.0,
        playing_camelot="8B",
        max_bpm_stretch=0.25,  # ±25%
        exclude_ids={"playing"},
    )
    ids = [t.track_id for t in out]
    assert "far_bpm" in ids
