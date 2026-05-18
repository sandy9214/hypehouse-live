"""Loudness leveler tests.

Cover:

1. :func:`compute_lufs` — file load + pyloudnorm wiring. Mocks
   ``librosa.load`` + ``pyloudnorm.Meter`` so the test is hermetic
   (no real decode, no real BS.1770 filter pass — those belong in
   an integration test against a known signal).
2. :func:`gain_db_for_target` — the dB-from-LUFS math + the clamps
   around silence / loud-master edge cases.
3. The ``add_track_from_path`` integration: loudness columns get
   populated from the lazy ``compute_lufs`` call. Mocks both the
   analyzer and the loudness module so the test is independent of
   librosa / pyloudnorm + the click-track WAV used elsewhere.
4. The v6 → v7 schema migration: a pre-PR DB opens cleanly, gains
   the two new columns, and reads back legacy rows with NULL
   loudness fields (= None on the dataclass).
"""
from __future__ import annotations

import math
import sqlite3
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace
from unittest.mock import MagicMock, patch

import pytest

from copilot.library import (
    LOUDNESS_TARGET_LUFS,
    TRACK_SCHEMA_VERSION,
    TrackLibrary,
    TrackRef,
)
from copilot.loudness import (
    DEFAULT_TARGET_LUFS,
    LUFS_MAX_REASONABLE,
    LUFS_SILENCE_FLOOR,
    MAX_GAIN_DB,
    MIN_GAIN_DB,
    compute_lufs,
    gain_db_for_target,
)


# --------------------------------------------------------------------- #
# 1. compute_lufs — mocked decode + meter                              #
# --------------------------------------------------------------------- #


def _install_fake_pyloudnorm(monkeypatch, integrated_loudness: float) -> None:
    """Stand up a stub ``pyloudnorm`` module so the lazy import inside
    :func:`compute_lufs` resolves to a controlled fake.

    The real pyloudnorm pulls scipy filter design + a 400 ms BS.1770
    gating loop; we don't want to exercise either inside a unit test.
    The stub mirrors only the surface the function uses:
    ``Meter(sr).integrated_loudness(samples) -> float``.
    """
    fake = ModuleType("pyloudnorm")

    class _Meter:
        def __init__(self, sr: int) -> None:
            self.sr = sr

        def integrated_loudness(self, _samples) -> float:
            return integrated_loudness

    fake.Meter = _Meter  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "pyloudnorm", fake)


def _install_fake_librosa(monkeypatch, samples, sr: int) -> None:
    """Stand up a stub ``librosa.load`` that returns the supplied buffer.

    Skips the real numba JIT cold-start (~3s) + the actual decode.
    Only ``librosa.load`` is referenced inside :func:`compute_lufs`, so
    the stub doesn't need to surface anything else.
    """
    fake = ModuleType("librosa")
    fake.load = MagicMock(return_value=(samples, sr))  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "librosa", fake)


def test_compute_lufs_returns_meter_value(monkeypatch, tmp_path: Path) -> None:
    """Stub the meter to return -14 LUFS; the function should pass that
    through verbatim."""
    import numpy as np

    _install_fake_librosa(monkeypatch, np.zeros(22050, dtype=np.float32), 44100)
    _install_fake_pyloudnorm(monkeypatch, -14.0)
    wav = tmp_path / "stub.wav"
    wav.touch()  # librosa is mocked; the file just needs to exist for Path
    assert compute_lufs(wav) == pytest.approx(-14.0)


def test_compute_lufs_quiet_track_returns_negative_value(
    monkeypatch, tmp_path: Path
) -> None:
    """A jazz-era master at -23 LUFS should round-trip as -23 LUFS."""
    import numpy as np

    _install_fake_librosa(monkeypatch, np.zeros(22050, dtype=np.float32), 44100)
    _install_fake_pyloudnorm(monkeypatch, -23.5)
    wav = tmp_path / "jazz.wav"
    wav.touch()
    assert compute_lufs(wav) == pytest.approx(-23.5)


def test_compute_lufs_transposes_stereo_buffer(
    monkeypatch, tmp_path: Path
) -> None:
    """librosa returns ``(channels, samples)`` for stereo loads;
    pyloudnorm wants ``(samples, channels)``. The function must
    transpose before calling ``integrated_loudness`` or the meter
    sees a mis-shaped array and returns a misleading result."""
    import numpy as np

    # Two channels of 1024 samples — shape librosa hands us.
    samples = np.zeros((2, 1024), dtype=np.float32)
    samples[0, 0] = 0.5  # arbitrary non-zero to make shape detection visible
    _install_fake_librosa(monkeypatch, samples, 44100)

    seen_shape: list[tuple[int, ...]] = []

    fake = ModuleType("pyloudnorm")

    class _Meter:
        def __init__(self, sr: int) -> None:
            self.sr = sr

        def integrated_loudness(self, buf) -> float:
            seen_shape.append(tuple(buf.shape))
            return -14.0

    fake.Meter = _Meter  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "pyloudnorm", fake)

    wav = tmp_path / "stereo.wav"
    wav.touch()
    compute_lufs(wav)
    # After transpose: (1024 samples, 2 channels).
    assert seen_shape == [(1024, 2)]


# --------------------------------------------------------------------- #
# 2. gain_db_for_target — pure-math edge cases                          #
# --------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "measured,expected",
    [
        (-14.0, 0.0),    # at target → no change
        (-23.0, 9.0),    # quiet jazz → +9 dB boost
        (-8.0, -6.0),    # loud EDM → -6 dB cut
        (-20.0, 6.0),    # streaming-loud master → modest boost
        (-30.0, 14.0),   # very quiet → boost saturates at MAX_GAIN_DB
        (-40.0, 14.0),   # extremely quiet → still capped
        (0.0, -14.0),    # broadcast-loud → -14 dB cut
        # Impossibly hot (above LUFS_MAX_REASONABLE = +3): measurement
        # is pre-clamped to +3, so the resulting gain is target(-14)
        # - +3 = -17 dB — *not* MIN_GAIN_DB, because the input cap
        # bounded the math before the output cap had to engage.
        (6.0, -17.0),
    ],
)
def test_gain_db_for_target_typical_signal(
    measured: float, expected: float
) -> None:
    """Walk the dB-from-LUFS arithmetic through quiet → loud examples."""
    assert gain_db_for_target(measured) == pytest.approx(expected, abs=1e-6)


def test_gain_db_for_target_silence_returns_zero() -> None:
    """``-inf`` (pyloudnorm's silence value) must NOT produce a +inf
    gain — that would explode the audio thread's per-sample multiply.
    Clamp to 0 dB (= passthrough) instead."""
    assert gain_db_for_target(float("-inf")) == 0.0


def test_gain_db_for_target_nan_returns_zero() -> None:
    """A NaN measurement (decoder edge case) must not propagate into
    the gain — same passthrough fallback as silence."""
    assert gain_db_for_target(float("nan")) == 0.0


def test_gain_db_for_target_below_silence_floor_returns_zero() -> None:
    """Values below :data:`LUFS_SILENCE_FLOOR` are effectively silence
    (pyloudnorm sometimes returns ``-120`` rather than ``-inf`` on
    short blocks)."""
    assert gain_db_for_target(LUFS_SILENCE_FLOOR - 5.0) == 0.0


def test_gain_db_for_target_above_max_reasonable_caps_measurement() -> None:
    """A measurement above :data:`LUFS_MAX_REASONABLE` is almost
    certainly a bug or synthetic signal — cap the input before the
    subtraction so we don't mute the deck on a +20 LUFS reading.

    With the cap engaged, the output is bounded by ``target -
    LUFS_MAX_REASONABLE`` (= -14 - 3 = -17 dB at the default target),
    which is *not* MIN_GAIN_DB (-20) but still inside the safe deck
    envelope. The point of the input cap is to keep us in that
    envelope without relying on the output clamp."""
    out = gain_db_for_target(LUFS_MAX_REASONABLE + 50.0)
    expected = DEFAULT_TARGET_LUFS - LUFS_MAX_REASONABLE
    assert out == pytest.approx(expected)
    # And the result is still inside the deck-safe range.
    assert MIN_GAIN_DB <= out <= MAX_GAIN_DB


def test_gain_db_for_target_custom_reference() -> None:
    """Apple Music targets -16 LUFS; verify the custom-target arg
    overrides the default."""
    assert gain_db_for_target(-20.0, target_lufs=-16.0) == pytest.approx(4.0)


def test_default_target_matches_library_constant() -> None:
    """The loudness module's default and the library's re-exported
    constant must agree — otherwise the wire shape silently disagrees
    with the engine's expectation."""
    assert DEFAULT_TARGET_LUFS == LOUDNESS_TARGET_LUFS
    assert DEFAULT_TARGET_LUFS == -14.0


# --------------------------------------------------------------------- #
# 3. add_track_from_path integration                                    #
# --------------------------------------------------------------------- #


def _stub_analysis(*, bpm: float = 120.0) -> SimpleNamespace:
    """Minimal shape ``copilot.vendor.analyzer.analyze`` returns."""
    return SimpleNamespace(
        path="/stub",
        duration=30.0,
        sr=22050,
        bpm=bpm,
        beats=[0.0, 0.5, 1.0],
        key="C",
        mode="maj",
        camelot="8B",
        energy=0.2,
        bpm_norm=bpm,
        onset_env=[],
        onset_hop_sec=1.0,
        downbeats=[0.0],
        beat_source="stub",
        segments=[],
        drop_times=[],
        buildup_starts=[],
        energy_profile={},
    )


def test_add_track_from_path_populates_loudness_columns(
    tmp_path: Path,
) -> None:
    """A successful loudness pass writes both ``lufs`` and
    ``track_gain_db`` on the new row, derived from the measured value
    (-20 LUFS → +6 dB toward the -14 LUFS reference)."""
    wav = tmp_path / "song.wav"
    wav.write_bytes(b"\0")  # content irrelevant — analyzer + loudness mocked

    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        with patch(
            "copilot.vendor.analyzer.analyze",
            return_value=_stub_analysis(),
        ), patch(
            "copilot.loudness.compute_lufs", return_value=-20.0
        ):
            ref = lib.add_track_from_path(wav, track_id="song")
        assert ref.lufs == pytest.approx(-20.0)
        # target(-14) - measured(-20) = +6 dB boost.
        assert ref.track_gain_db == pytest.approx(6.0)
        # Round-trip through the public read path.
        got = lib.get("song")
        assert got is not None
        assert got.lufs == pytest.approx(-20.0)
        assert got.track_gain_db == pytest.approx(6.0)
    finally:
        lib.close()


def test_add_track_from_path_handles_loudness_failure_gracefully(
    tmp_path: Path,
) -> None:
    """A pyloudnorm/librosa error inside :func:`compute_lufs` must NOT
    abort the ingest — the row lands with NULL loudness fields and
    the engine treats it as 0 dB gain (passthrough)."""
    wav = tmp_path / "broken.wav"
    wav.write_bytes(b"\0")

    lib = TrackLibrary(tmp_path / "lib.db")
    try:
        with patch(
            "copilot.vendor.analyzer.analyze",
            return_value=_stub_analysis(),
        ), patch(
            "copilot.loudness.compute_lufs",
            side_effect=RuntimeError("simulated decode failure"),
        ):
            ref = lib.add_track_from_path(wav, track_id="broken")
        # Ingest still produces a valid row.
        assert ref.lufs is None
        assert ref.track_gain_db is None
        got = lib.get("broken")
        assert got is not None
        assert got.lufs is None
        assert got.track_gain_db is None
        # And the BPM / camelot fields still come through — loudness
        # failure isolates from the rest of the analysis.
        assert got.camelot_key == "8B"
        assert math.isclose(got.bpm, 120.0)
    finally:
        lib.close()


# --------------------------------------------------------------------- #
# 4. Schema migration v6 → v7                                          #
# --------------------------------------------------------------------- #


def test_migration_v6_to_v7_adds_loudness_columns(tmp_path: Path) -> None:
    """A v6-shaped DB (no ``lufs`` / ``track_gain_db`` columns) reopened
    by the v7 library must gain both columns + bump the version stamp.
    The legacy row reads back with ``lufs=None`` + ``track_gain_db=None``
    (the documented "no measurement" state)."""
    db = tmp_path / "v6.db"
    # Hand-roll a v6-shaped DB so the migration path is exercised end-
    # to-end. The v6 schema lacks both loudness columns — every other
    # v6 column is present so the migration ALTER TABLE branches don't
    # need to fire.
    conn = sqlite3.connect(str(db))
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
            downbeats_json TEXT NOT NULL DEFAULT '[]',
            hot_cues_json TEXT NOT NULL
                DEFAULT '[null,null,null,null,null,null,null,null]',
            waveform_peaks BLOB,
            stems_dir TEXT,
            stems_status TEXT
        );
        CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
        INSERT INTO schema_version (version) VALUES (6);
        INSERT INTO tracks
            (track_id, path, bpm, camelot_key, energy, duration_s)
            VALUES ('legacy', '/legacy.mp3', 120.0, '8B', 0.2, 200.0);
        """
    )
    conn.commit()
    conn.close()

    lib = TrackLibrary(db)
    try:
        cols = {
            row["name"]
            for row in lib._conn.execute("PRAGMA table_info(tracks)")
        }
        assert "lufs" in cols
        assert "track_gain_db" in cols
        version = lib._conn.execute(
            "SELECT version FROM schema_version"
        ).fetchone()
        assert version["version"] == TRACK_SCHEMA_VERSION
        legacy = lib.get("legacy")
        assert legacy is not None
        # Legacy row: loudness fields stay NULL — we don't retro-
        # analyze on upgrade (would block reopen on the entire
        # library's librosa cold-start tax).
        assert legacy.lufs is None
        assert legacy.track_gain_db is None
    finally:
        lib.close()


def test_add_track_persists_loudness_round_trip(tmp_path: Path) -> None:
    """A pre-built ``TrackRef`` carrying loudness data round-trips
    through ``add_track`` → ``get`` without value drift."""
    lib = TrackLibrary(":memory:")
    try:
        ref = TrackRef(
            track_id="hot",
            path="/hot.mp3",
            bpm=128.0,
            camelot_key="8B",
            energy=0.3,
            duration_s=180.0,
            lufs=-9.5,
            track_gain_db=-4.5,
        )
        lib.add_track(ref)
        got = lib.get("hot")
        assert got is not None
        assert got.lufs == pytest.approx(-9.5)
        assert got.track_gain_db == pytest.approx(-4.5)
    finally:
        lib.close()


def test_track_ref_to_wire_includes_loudness_fields() -> None:
    """The wire projection (UI / engine consumer) must surface
    ``lufs`` + ``track_gain_db`` so the engine's DeckLoad event can
    pick them up. A track without loudness data serializes as
    ``null`` (JSON-friendly) — *not* the dataclass's ``None``
    sentinel, which the JSON encoder coerces correctly."""
    from copilot.library_rpc import track_ref_to_wire

    ref = TrackRef(
        track_id="t",
        path="/t.mp3",
        bpm=120.0,
        camelot_key="8B",
        energy=0.2,
        duration_s=200.0,
        lufs=-18.0,
        track_gain_db=4.0,
    )
    wire = track_ref_to_wire(ref)
    assert wire["lufs"] == pytest.approx(-18.0)
    assert wire["track_gain_db"] == pytest.approx(4.0)

    # Track without a loudness pass — both fields surface as None.
    bare = TrackRef("bare", "/b.mp3", 120.0, "8B", 0.2, 200.0)
    bare_wire = track_ref_to_wire(bare)
    assert bare_wire["lufs"] is None
    assert bare_wire["track_gain_db"] is None
