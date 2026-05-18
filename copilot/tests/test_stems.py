"""Stem-separation scaffold tests.

Demucs itself is heavy (~2 GB wheel + ~2 GB model checkpoint) and we
have no business invoking it in CI. Every test in this module mocks at
the ``copilot.stems`` boundary so the runtime stays sub-second.

Coverage map:

* :func:`compute_stems` wiring — calls demucs API with the expected
  args, returns the 4-stem dict, short-circuits on cache hits.
* :class:`TrackLibrary` schema v5 migration — fresh DB + ALTER on v4.
* :meth:`TrackLibrary.compute_track_stems` — status transitions
  ``pending -> ready`` on success, ``pending -> failed`` on error.
* :class:`LibraryRpcHandler` ``library.compute_stems`` /
  ``library.get_stems`` wire shapes, plus the optional-dep error
  branch.
"""
from __future__ import annotations

import sqlite3
import sys
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from copilot.library import (
    STEMS_STATUS_FAILED,
    STEMS_STATUS_PENDING,
    STEMS_STATUS_READY,
    TRACK_SCHEMA_VERSION,
    TrackLibrary,
    TrackRef,
)
from copilot.library_rpc import (
    JSONRPC_FEATURE_NOT_INSTALLED,
    JSONRPC_INVALID_PARAMS,
    LibraryRpcHandler,
    RpcError,
)
from copilot import stems as stems_mod
from copilot.stems import STEM_NAMES, StemsDependencyError, compute_stems

_asyncio = pytest.mark.asyncio


# ---------------------------------------------------------------------
# compute_stems — module boundary
# ---------------------------------------------------------------------


def _make_audio_file(tmp_path: Path, name: str = "track.mp3") -> Path:
    """Create a non-empty stub audio file. Demucs is mocked so the
    bytes don't have to be valid PCM."""
    p = tmp_path / name
    p.write_bytes(b"fake audio bytes")
    return p


def _fake_demucs_module(captured: dict[str, Any]) -> Any:
    """Build a stand-in for ``demucs.api`` that records its inputs.

    Returned tuple shape mirrors the real ``Separator.separate_audio_file``:
    ``(origin_tensor, {stem_name: tensor})``.
    """

    class _Sep:
        def __init__(self, *, model: str) -> None:
            captured["model"] = model
            self.samplerate = 44100

        def separate_audio_file(self, path: str) -> tuple[object, dict[str, object]]:
            captured["audio_path"] = path
            return ("ORIGIN", {n: f"TENSOR_{n}" for n in STEM_NAMES})

    def _save_audio(tensor: object, path: str, *, samplerate: int) -> None:
        captured.setdefault("saved", []).append((tensor, path, samplerate))
        # Drop a real (tiny) byte payload so cache_is_complete sees a
        # non-zero file.
        Path(path).write_bytes(b"WAV-stub")

    return SimpleNamespace(Separator=_Sep, save_audio=_save_audio)


def test_compute_stems_invokes_demucs_with_expected_args(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    audio = _make_audio_file(tmp_path)
    out_dir = tmp_path / "stems-out"
    captured: dict[str, Any] = {}

    fake = _fake_demucs_module(captured)
    monkeypatch.setitem(sys.modules, "demucs", SimpleNamespace(api=fake))
    monkeypatch.setitem(sys.modules, "demucs.api", fake)

    result = compute_stems(audio, out_dir)

    # Right model.
    assert captured["model"] == "htdemucs"
    # Right input path (str-cast, demucs API takes a string).
    assert captured["audio_path"] == str(audio)
    # Four save calls — one per stem, in canonical order.
    assert [name for _, p, _ in captured["saved"] for name in [Path(p).stem]] == list(STEM_NAMES)
    # Returned dict has all four stems and points at on-disk files.
    assert set(result) == set(STEM_NAMES)
    for name, path in result.items():
        assert path == out_dir / f"{name}.wav"
        assert path.exists() and path.stat().st_size > 0


def test_compute_stems_short_circuits_on_cache_hit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Pre-populate the four output WAVs — demucs must not be touched."""
    audio = _make_audio_file(tmp_path)
    out_dir = tmp_path / "stems-out"
    out_dir.mkdir()
    for name in STEM_NAMES:
        (out_dir / f"{name}.wav").write_bytes(b"cached-wav")

    def _boom(*args: Any, **kwargs: Any) -> None:  # pragma: no cover
        raise AssertionError("demucs must not be imported on cache hit")

    monkeypatch.setitem(
        sys.modules,
        "demucs",
        SimpleNamespace(api=SimpleNamespace(Separator=_boom, save_audio=_boom)),
    )
    monkeypatch.setitem(
        sys.modules,
        "demucs.api",
        SimpleNamespace(Separator=_boom, save_audio=_boom),
    )

    result = compute_stems(audio, out_dir)
    assert set(result) == set(STEM_NAMES)


def test_compute_stems_raises_dependency_error_when_demucs_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    audio = _make_audio_file(tmp_path)
    out_dir = tmp_path / "stems-out"

    # Block the import: pretend demucs is not installed.
    monkeypatch.setitem(sys.modules, "demucs", None)
    monkeypatch.setitem(sys.modules, "demucs.api", None)

    with pytest.raises(StemsDependencyError) as exc:
        compute_stems(audio, out_dir)
    assert "pip install hypehouse-copilot[stems]" in str(exc.value)


def test_compute_stems_missing_audio_raises(tmp_path: Path) -> None:
    with pytest.raises(FileNotFoundError):
        compute_stems(tmp_path / "nope.mp3", tmp_path / "out")


# ---------------------------------------------------------------------
# Schema migration v4 -> v5
# ---------------------------------------------------------------------


def test_schema_migration_adds_stems_columns(tmp_path: Path) -> None:
    """Build a v4-shaped DB by hand, then open with TrackLibrary and
    verify the ALTER TABLE migration adds stems_dir + stems_status."""
    db = tmp_path / "old.db"
    # Synthesize a v4 schema — same columns as today minus the two v5
    # additions. The version row is intentionally set to 4 so we can
    # verify it bumps to 5 after migration.
    conn = sqlite3.connect(str(db))
    conn.executescript(
        """
        CREATE TABLE tracks (
            track_id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            bpm REAL NOT NULL,
            camelot_key TEXT NOT NULL,
            energy REAL NOT NULL,
            duration_s REAL NOT NULL,
            beat_grid_anchor_ms INTEGER NOT NULL DEFAULT 0,
            beat_period_ms REAL NOT NULL DEFAULT 500.0,
            downbeats_json TEXT NOT NULL DEFAULT '[]',
            hot_cues_json TEXT NOT NULL DEFAULT '[null,null,null,null,null,null,null,null]',
            waveform_peaks BLOB
        );
        CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
        INSERT INTO schema_version (version) VALUES (4);
        INSERT INTO tracks (track_id, path, bpm, camelot_key, energy, duration_s)
        VALUES ('legacy', '/legacy.mp3', 120.0, '8B', 0.2, 200.0);
        """
    )
    conn.commit()
    conn.close()

    lib = TrackLibrary(db)
    try:
        cols = {r[1] for r in lib._conn.execute("PRAGMA table_info(tracks)")}
        assert "stems_dir" in cols
        assert "stems_status" in cols

        # Version stamped to current.
        ver = lib._conn.execute("SELECT version FROM schema_version").fetchone()
        assert ver[0] == TRACK_SCHEMA_VERSION == 5

        # Legacy row survived migration with NULL stems columns.
        row = lib._conn.execute(
            "SELECT stems_dir, stems_status FROM tracks WHERE track_id = 'legacy'"
        ).fetchone()
        assert row[0] is None
        assert row[1] is None
    finally:
        lib.close()


def test_get_stems_status_returns_none_for_unknown_track(
    library: TrackLibrary,
) -> None:
    assert library.get_stems_status("does-not-exist") is None


def test_set_stems_persists_status_and_dir(library: TrackLibrary) -> None:
    library.add_track(TrackRef("t1", "/t.mp3", 120.0, "8B", 0.2, 200.0))
    library.set_stems("t1", status=STEMS_STATUS_READY, stems_dir="/cache/t1")
    assert library.get_stems_status("t1") == (STEMS_STATUS_READY, "/cache/t1")


def test_set_stems_raises_for_missing_track(library: TrackLibrary) -> None:
    with pytest.raises(KeyError):
        library.set_stems("ghost", status=STEMS_STATUS_READY, stems_dir=None)


# ---------------------------------------------------------------------
# TrackLibrary.compute_track_stems status transitions
# ---------------------------------------------------------------------


def test_compute_track_stems_ready_on_success(
    library: TrackLibrary,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    library.add_track(TrackRef("t1", "/t.mp3", 120.0, "8B", 0.2, 200.0))

    captured: dict[str, Any] = {}

    def _fake_compute(audio_path: Path, output_dir: Path) -> dict[str, Path]:
        captured["audio_path"] = audio_path
        captured["output_dir"] = output_dir
        output_dir.mkdir(parents=True, exist_ok=True)
        result = {}
        for n in STEM_NAMES:
            wav = output_dir / f"{n}.wav"
            wav.write_bytes(b"stub")
            result[n] = wav
        return result

    monkeypatch.setattr(stems_mod, "compute_stems", _fake_compute)

    result = library.compute_track_stems("t1", output_root=tmp_path)

    # Sanity: passthrough args.
    assert captured["audio_path"] == Path("/t.mp3")
    assert captured["output_dir"] == tmp_path / "t1"
    # Persisted status.
    status, stems_dir = library.get_stems_status("t1")  # type: ignore[misc]
    assert status == STEMS_STATUS_READY
    assert stems_dir == str(tmp_path / "t1")
    # Returned dict.
    assert set(result) == set(STEM_NAMES)


def test_compute_track_stems_failed_on_error(
    library: TrackLibrary,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    library.add_track(TrackRef("t1", "/t.mp3", 120.0, "8B", 0.2, 200.0))

    def _boom(audio_path: Path, output_dir: Path) -> dict[str, Path]:
        raise RuntimeError("demucs blew up")

    monkeypatch.setattr(stems_mod, "compute_stems", _boom)

    with pytest.raises(RuntimeError, match="demucs blew up"):
        library.compute_track_stems("t1", output_root=tmp_path)

    status, stems_dir = library.get_stems_status("t1")  # type: ignore[misc]
    assert status == STEMS_STATUS_FAILED
    # Even on failure we record the intended cache dir so a retry has
    # the same target.
    assert stems_dir == str(tmp_path / "t1")


def test_compute_track_stems_raises_for_unknown_track(
    library: TrackLibrary, tmp_path: Path
) -> None:
    with pytest.raises(KeyError):
        library.compute_track_stems("ghost", output_root=tmp_path)


# ---------------------------------------------------------------------
# RPC: library.compute_stems
# ---------------------------------------------------------------------


def _install_fake_demucs(monkeypatch: pytest.MonkeyPatch) -> None:
    """Make ``import demucs.api`` succeed regardless of whether the
    real package is installed. We patch the *library*-level
    ``compute_stems`` separately so the fake module is only used for
    the import probe in the RPC handler."""
    fake_api = SimpleNamespace(Separator=object, save_audio=lambda *a, **k: None)
    monkeypatch.setitem(sys.modules, "demucs", SimpleNamespace(api=fake_api))
    monkeypatch.setitem(sys.modules, "demucs.api", fake_api)


@_asyncio
async def test_rpc_compute_stems_returns_pending_and_runs_task(
    library: TrackLibrary,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    library.add_track(TrackRef("t1", "/t.mp3", 120.0, "8B", 0.2, 200.0))
    _install_fake_demucs(monkeypatch)

    # Override the heavy compute with a fast async-friendly stub.
    def _fake_compute(audio_path: Path, output_dir: Path) -> dict[str, Path]:
        output_dir.mkdir(parents=True, exist_ok=True)
        out: dict[str, Path] = {}
        for n in STEM_NAMES:
            wav = output_dir / f"{n}.wav"
            wav.write_bytes(b"stub")
            out[n] = wav
        return out

    monkeypatch.setattr(stems_mod, "compute_stems", _fake_compute)
    # Force the default root into the tmp dir so we don't write to
    # ~/.local/share during tests.
    monkeypatch.setattr(stems_mod, "default_stems_root", lambda: tmp_path)

    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.compute_stems", {"track_id": "t1"}
    )
    assert result == {"track_id": "t1", "status": STEMS_STATUS_PENDING}

    # Drain the background task — gives the asyncio loop a chance to
    # run the worker we just spawned. Without this the status would
    # still be "pending" when get_stems polls below.
    task = handler._stem_tasks.get("t1")
    if task is not None:
        await task

    status, _dir = library.get_stems_status("t1")  # type: ignore[misc]
    assert status == STEMS_STATUS_READY


@_asyncio
async def test_rpc_compute_stems_returns_install_hint_when_dep_missing(
    library: TrackLibrary,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    library.add_track(TrackRef("t1", "/t.mp3", 120.0, "8B", 0.2, 200.0))
    # Mark demucs.api as unimportable.
    monkeypatch.setitem(sys.modules, "demucs", None)
    monkeypatch.setitem(sys.modules, "demucs.api", None)

    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch("library.compute_stems", {"track_id": "t1"})
    assert exc.value.code == JSONRPC_FEATURE_NOT_INSTALLED
    assert "pip install hypehouse-copilot[stems]" in exc.value.message


@_asyncio
async def test_rpc_compute_stems_unknown_track(
    library: TrackLibrary, monkeypatch: pytest.MonkeyPatch
) -> None:
    _install_fake_demucs(monkeypatch)
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch("library.compute_stems", {"track_id": "ghost"})
    assert exc.value.code == JSONRPC_INVALID_PARAMS


# ---------------------------------------------------------------------
# RPC: library.get_stems
# ---------------------------------------------------------------------


@_asyncio
async def test_rpc_get_stems_returns_null_for_missing_track(
    library: TrackLibrary,
) -> None:
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.get_stems", {"track_id": "does-not-exist"}
    )
    assert result == {
        "track_id": "does-not-exist",
        "status": None,
        "stems": None,
    }


@_asyncio
async def test_rpc_get_stems_returns_ready_paths(
    library: TrackLibrary, tmp_path: Path
) -> None:
    library.add_track(TrackRef("t1", "/t.mp3", 120.0, "8B", 0.2, 200.0))
    cache_dir = tmp_path / "t1"
    cache_dir.mkdir()
    for name in STEM_NAMES:
        (cache_dir / f"{name}.wav").write_bytes(b"stub")
    library.set_stems("t1", status=STEMS_STATUS_READY, stems_dir=str(cache_dir))

    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.get_stems", {"track_id": "t1"}
    )
    assert result["status"] == STEMS_STATUS_READY
    assert result["track_id"] == "t1"
    assert set(result["stems"]) == set(STEM_NAMES)
    for name, path in result["stems"].items():
        assert Path(path) == cache_dir / f"{name}.wav"


@_asyncio
async def test_rpc_get_stems_flips_to_failed_when_files_disappear(
    library: TrackLibrary, tmp_path: Path
) -> None:
    """If status says READY but a stem WAV is missing on disk, the
    handler should flip the row to "failed" and return null stems."""
    library.add_track(TrackRef("t1", "/t.mp3", 120.0, "8B", 0.2, 200.0))
    cache_dir = tmp_path / "t1"
    cache_dir.mkdir()
    # Only write three of four — the fourth is "missing".
    for name in STEM_NAMES[:3]:
        (cache_dir / f"{name}.wav").write_bytes(b"stub")
    library.set_stems("t1", status=STEMS_STATUS_READY, stems_dir=str(cache_dir))

    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.get_stems", {"track_id": "t1"}
    )
    assert result["status"] == STEMS_STATUS_FAILED
    assert result["stems"] is None
    # Persisted state matches the wire response.
    status, _dir = library.get_stems_status("t1")  # type: ignore[misc]
    assert status == STEMS_STATUS_FAILED


@_asyncio
async def test_rpc_get_stems_returns_null_when_never_requested(
    library: TrackLibrary,
) -> None:
    library.add_track(TrackRef("t1", "/t.mp3", 120.0, "8B", 0.2, 200.0))
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "library.get_stems", {"track_id": "t1"}
    )
    assert result == {"track_id": "t1", "status": None, "stems": None}


# ---------------------------------------------------------------------
# Handler registration
# ---------------------------------------------------------------------


def test_handler_advertises_new_methods(library: TrackLibrary) -> None:
    handler = LibraryRpcHandler(library)
    fq = handler.fully_qualified_methods
    assert "library.compute_stems" in fq
    assert "library.get_stems" in fq
    assert handler.handles("library.compute_stems")
    assert handler.handles("library.get_stems")
