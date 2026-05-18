"""Track library — SQLite-backed catalog of analyzed tracks.

The library stores per-track features:

* BPM + Camelot key (compatibility gates for the proposer).
* Energy + duration (mashability score + UI display).
* Beat-grid anchor + beat period (engine `DeckLoad` event payload).
* Downbeats (JSON-encoded list of ms positions) — drives phrase-aligned
  transitions in the proposer + engine. See ``docs/api/ws-protocol.md``
  "Beat-grid + downbeats" for the alignment model.

Schema::

    CREATE TABLE tracks (
        track_id            TEXT PRIMARY KEY,
        path                TEXT NOT NULL,
        bpm                 REAL NOT NULL,
        camelot_key         TEXT NOT NULL,
        energy              REAL NOT NULL,
        duration_s          REAL NOT NULL,
        beat_grid_anchor_ms INTEGER NOT NULL DEFAULT 0,
        beat_period_ms      REAL NOT NULL DEFAULT 500.0,
        downbeats_json      TEXT NOT NULL DEFAULT '[]'
    );

Schema migration: ``TRACK_SCHEMA_VERSION`` is bumped whenever the columns
change. ``_init_schema`` reads the current version from the
``schema_version`` PRAGMA-style table and ALTERs missing columns in place
so existing local DBs keep working without a full rebuild.

Analysis path: ``add_track_from_path(path)`` invokes the vendored v1
analyzer (madmom DBNBeatTracker for beats/downbeats, falls back to
librosa) and persists the result. Tests bypass this slow path with
``add_track`` which accepts a pre-built ``TrackRef``.

The default DB lives at ``~/.hypehouse-live/library.db``; tests inject a
``:memory:`` DB via the ``db_path`` constructor argument.
"""
from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass, field
from pathlib import Path

# Bumped to v2 in the beat-grid analysis PR (this one). Stamped into the
# DB on first init; migrations dispatch on the gap between this constant
# and the value recorded in the ``schema_version`` table.
TRACK_SCHEMA_VERSION = 2

# Camelot wheel ordering — same convention as HypeHouse v1 analyzer.py.
# Index = (number-1) * 2 + (0 if A else 1). Used for circular distance only.
_CAMELOT_NUMBERS = list(range(1, 13))  # 1..12


@dataclass(frozen=True)
class TrackRef:
    """A library entry, shape-compatible with what the engine expects.

    The first six fields are positional for backwards compatibility with
    pre-beat-grid call sites (``TrackRef(id, path, bpm, key, energy, dur)``).
    New beat-grid fields are keyword-only with sensible defaults so adding
    them didn't churn every test fixture.

    * ``beat_grid_anchor_ms``: first beat position in ms (= 0 for most
      tracks where the analyzer locks beat 0 to t=0).
    * ``beat_period_ms``: 60_000 / bpm. Derived but stored so the engine
      doesn't re-compute on every load.
    * ``downbeats_ms``: list of bar-start positions in ms. Empty list
      when analysis hasn't run yet — proposer falls back to current
      position in that case (see ``copilot.proposer.next_downbeat_after``).
    """

    track_id: str
    path: str
    bpm: float
    camelot_key: str  # e.g. "8B", "10A"
    energy: float  # 0..~1, RMS of the analyzed window
    duration_s: float
    beat_grid_anchor_ms: int = 0
    beat_period_ms: float = 500.0  # 120 BPM default
    downbeats_ms: list[int] = field(default_factory=list)


# Hard compatibility gates. Tracks outside these can sometimes mix but the
# v0.1 co-pilot doesn't try — better to reject than to surprise a host with
# a clashing key.
_MAX_BPM_STRETCH = 0.08  # ±8% — Pioneer DDJ default sync window
_MAX_KEY_DISTANCE = 2    # ≤2 = same / adjacent / relative / perfect 5th-ish


def camelot_distance(a: str, b: str) -> int:
    """Circular distance on the Camelot wheel, accounting for A/B (minor/major)
    sibling pairs.

    Definition:
        * 0 = identical key.
        * 1 = adjacent number, same letter (e.g. 8B → 9B) OR same number,
              different letter (e.g. 8B → 8A — relative minor/major).
        * 2 = two steps away (e.g. 8B → 10B or 8B → 9A).

    Unknown / malformed codes return a large sentinel (99) so they're filtered
    out by ``_MAX_KEY_DISTANCE`` automatically.
    """
    a = (a or "").strip().upper()
    b = (b or "").strip().upper()
    try:
        a_num = int(a[:-1])
        b_num = int(b[:-1])
        a_letter = a[-1]
        b_letter = b[-1]
        if a_letter not in ("A", "B") or b_letter not in ("A", "B"):
            return 99
        if not (1 <= a_num <= 12 and 1 <= b_num <= 12):
            return 99
    except (ValueError, IndexError):
        return 99

    # Circular number distance on the 12-step wheel.
    diff = abs(a_num - b_num)
    num_d = min(diff, 12 - diff)

    # Letter mismatch costs 1.
    letter_d = 0 if a_letter == b_letter else 1

    # Treat the special case "same number, different letter" (relative
    # minor/major) as distance 1 — not 0 + 1 = 1 (already correct) — and
    # "adjacent number, same letter" as 1 as well. The sum below works.
    return num_d + letter_d


def bpm_stretch_ratio(playing_bpm: float, candidate_bpm: float) -> float:
    """Relative stretch required to beatmatch candidate to playing track.

    Returns 0.0 if either BPM is non-positive (caller should reject)."""
    if playing_bpm <= 0 or candidate_bpm <= 0:
        return float("inf")
    return abs(candidate_bpm - playing_bpm) / playing_bpm


class TrackLibrary:
    """Read-only-ish library facade over a SQLite catalog.

    The co-pilot only *reads* this; writes happen via the v1 analyzer CLI or a
    separate ingestion process. We still expose ``add_track`` for tests and a
    future ingestion command.
    """

    def __init__(self, db_path: str | Path = "~/.hypehouse-live/library.db"):
        if isinstance(db_path, str) and db_path == ":memory:":
            self._db_path: str | Path = ":memory:"
            self._conn = sqlite3.connect(":memory:")
        else:
            resolved = Path(db_path).expanduser()
            resolved.parent.mkdir(parents=True, exist_ok=True)
            self._db_path = resolved
            self._conn = sqlite3.connect(str(resolved))
        self._conn.row_factory = sqlite3.Row
        self._init_schema()

    def _init_schema(self) -> None:
        # Base table — always create. New columns (beat_grid_anchor_ms,
        # beat_period_ms, downbeats_json) are migrated in via ALTER TABLE
        # below so older DBs (schema v1, pre this PR) upgrade in place
        # rather than getting recreated, which would lose user data.
        self._conn.executescript(
            """
            CREATE TABLE IF NOT EXISTS tracks (
                track_id    TEXT PRIMARY KEY,
                path        TEXT NOT NULL,
                bpm         REAL NOT NULL,
                camelot_key TEXT NOT NULL,
                energy      REAL NOT NULL,
                duration_s  REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY
            );
            CREATE INDEX IF NOT EXISTS tracks_bpm_idx ON tracks (bpm);
            CREATE INDEX IF NOT EXISTS tracks_key_idx ON tracks (camelot_key);
            """
        )
        # Add new columns idempotently. PRAGMA table_info is cheaper than
        # rescuing a sqlite3.OperationalError per ALTER attempt and is the
        # canonical SQLite migration pattern.
        cols = {
            row["name"]
            for row in self._conn.execute("PRAGMA table_info(tracks)")
        }
        if "beat_grid_anchor_ms" not in cols:
            self._conn.execute(
                "ALTER TABLE tracks "
                "ADD COLUMN beat_grid_anchor_ms INTEGER NOT NULL DEFAULT 0"
            )
        if "beat_period_ms" not in cols:
            self._conn.execute(
                "ALTER TABLE tracks "
                "ADD COLUMN beat_period_ms REAL NOT NULL DEFAULT 500.0"
            )
        if "downbeats_json" not in cols:
            self._conn.execute(
                "ALTER TABLE tracks "
                "ADD COLUMN downbeats_json TEXT NOT NULL DEFAULT '[]'"
            )
        # Stamp the current schema version. Multiple-row safety via
        # INSERT OR REPLACE; the table only ever holds one row.
        self._conn.execute(
            "INSERT OR REPLACE INTO schema_version (version) VALUES (?)",
            (TRACK_SCHEMA_VERSION,),
        )
        self._conn.commit()

    def close(self) -> None:
        self._conn.close()

    # --- write path (ingestion / tests) -----------------------------------

    def add_track(self, track: TrackRef) -> None:
        self._conn.execute(
            "INSERT OR REPLACE INTO tracks "
            "(track_id, path, bpm, camelot_key, energy, duration_s, "
            " beat_grid_anchor_ms, beat_period_ms, downbeats_json) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                track.track_id,
                track.path,
                float(track.bpm),
                track.camelot_key,
                float(track.energy),
                float(track.duration_s),
                int(track.beat_grid_anchor_ms),
                float(track.beat_period_ms),
                json.dumps([int(d) for d in track.downbeats_ms]),
            ),
        )
        self._conn.commit()

    def add_track_from_path(
        self,
        path: str | Path,
        *,
        track_id: str | None = None,
        cache_dir: Path | None = None,
        quick: bool = False,
    ) -> TrackRef:
        """Run the vendored v1 analyzer on ``path`` and persist the result.

        Heavy import path (librosa + optional madmom) — kept lazy so the
        co-pilot service can import :mod:`copilot.library` without
        paying the librosa cold-start tax. Tests mock at this boundary
        (see ``test_library_analysis.py``).

        ``cache_dir`` defaults to a ``cache/`` sibling of the SQLite DB so
        analyzer JSON survives library rebuilds.
        """
        # Lazy import — librosa pulls numba which takes ~3s to import.
        from .vendor.analyzer import analyze  # type: ignore[import-not-found]

        path_obj = Path(path)
        if cache_dir is None:
            if isinstance(self._db_path, str) and self._db_path == ":memory:":
                cache_dir = Path.cwd() / "_copilot_analysis_cache"
            else:
                cache_dir = Path(self._db_path).parent / "analysis_cache"
        cache_dir.mkdir(parents=True, exist_ok=True)
        analysis = analyze(path_obj, cache_dir, quick=quick)

        # Beat-grid anchor = first detected beat × 1000 (ms). If the
        # analyzer found no beats (silent track / decode failure), anchor
        # to 0 and let the engine fall back to its default grid.
        first_beat_s = (
            float(analysis.beats[0]) if analysis.beats else 0.0
        )
        beat_grid_anchor_ms = int(round(first_beat_s * 1000.0))

        safe_bpm = float(analysis.bpm) if analysis.bpm > 0 else 120.0
        beat_period_ms = 60_000.0 / safe_bpm

        downbeats_s = list(analysis.downbeats or [])
        downbeats_ms = [int(round(float(t) * 1000.0)) for t in downbeats_s]

        ref = TrackRef(
            track_id=track_id or path_obj.stem,
            path=str(path_obj),
            bpm=safe_bpm,
            camelot_key=analysis.camelot or "?",
            energy=float(analysis.energy),
            duration_s=float(analysis.duration),
            beat_grid_anchor_ms=beat_grid_anchor_ms,
            beat_period_ms=beat_period_ms,
            downbeats_ms=downbeats_ms,
        )
        self.add_track(ref)
        return ref

    # --- read path --------------------------------------------------------

    def all_tracks(self) -> list[TrackRef]:
        return [self._row_to_ref(r) for r in self._conn.execute("SELECT * FROM tracks")]

    def get(self, track_id: str) -> TrackRef | None:
        r = self._conn.execute(
            "SELECT * FROM tracks WHERE track_id = ?", (track_id,)
        ).fetchone()
        return self._row_to_ref(r) if r else None

    def count_tracks(self) -> int:
        """Total row count — used by paginated ``list_tracks`` for the UI."""
        r = self._conn.execute("SELECT COUNT(*) AS n FROM tracks").fetchone()
        return int(r["n"]) if r else 0

    def list_tracks(
        self, *, limit: int = 100, offset: int = 0
    ) -> list[TrackRef]:
        """Paginated catalog dump — ordered by track_id for stable scroll.

        ``limit`` clamped to 1..1000; ``offset`` clamped to >=0. Both
        clamps are silent because the UI library panel uses a fixed
        page size and out-of-range offsets are an honest empty result
        (last page reached) rather than an error.
        """
        limit = max(1, min(int(limit), 1000))
        offset = max(0, int(offset))
        rows = self._conn.execute(
            "SELECT * FROM tracks ORDER BY track_id LIMIT ? OFFSET ?",
            (limit, offset),
        ).fetchall()
        return [self._row_to_ref(r) for r in rows]

    def search_tracks(
        self,
        query: str,
        *,
        limit: int = 100,
    ) -> list[TrackRef]:
        """Substring + key + BPM-range search over the catalog.

        Supports three shorthand syntaxes layered on top of the
        default substring match:

        * ``key:8B`` — exact Camelot key match (case-insensitive).
        * ``bpm:120-130`` — inclusive BPM range filter.
        * everything else — case-insensitive substring matched against
          the ``track_id`` and ``path`` columns (which include the
          filename and so cover title / artist for filename-tagged
          libraries — v0.1's only labeling source).

        Multiple tokens AND together. Empty query returns the first
        ``limit`` rows of the library (same as ``list_tracks``).
        """
        tokens = (query or "").strip().split()
        if not tokens:
            return self.list_tracks(limit=limit, offset=0)

        clauses: list[str] = []
        params: list[object] = []
        for tok in tokens:
            tok_lc = tok.lower()
            if tok_lc.startswith("key:"):
                clauses.append("LOWER(camelot_key) = ?")
                params.append(tok_lc[4:])
            elif tok_lc.startswith("bpm:") and "-" in tok_lc[4:]:
                lo_s, hi_s = tok_lc[4:].split("-", 1)
                try:
                    lo = float(lo_s)
                    hi = float(hi_s)
                except ValueError:
                    # Malformed range — degrade to substring on the raw token.
                    clauses.append(
                        "(LOWER(track_id) LIKE ? OR LOWER(path) LIKE ?)"
                    )
                    like = f"%{tok_lc}%"
                    params.extend([like, like])
                    continue
                clauses.append("bpm BETWEEN ? AND ?")
                params.extend([lo, hi])
            else:
                clauses.append(
                    "(LOWER(track_id) LIKE ? OR LOWER(path) LIKE ?)"
                )
                like = f"%{tok_lc}%"
                params.extend([like, like])

        where = " AND ".join(clauses)
        sql = (
            "SELECT * FROM tracks "
            f"WHERE {where} "
            "ORDER BY track_id LIMIT ?"
        )
        params.append(max(1, min(int(limit), 1000)))
        rows = self._conn.execute(sql, params).fetchall()
        return [self._row_to_ref(r) for r in rows]

    def add_tracks_from_directory(
        self,
        directory: str | Path,
        *,
        exts: tuple[str, ...] = (".mp3", ".wav", ".flac", ".m4a", ".aac", ".ogg"),
        quick: bool = True,
    ) -> list[TrackRef]:
        """Recursively scan ``directory`` and analyze every supported file.

        Used by the UI's "library is empty" empty-state — operator runs
        ``python -m copilot.library scan /path/to/music`` (or the
        ``library.add_track_from_directory`` RPC) once and the panel
        populates. Files already in the library (matched by
        ``track_id == Path.stem``) are skipped to keep the scan
        idempotent.

        ``quick=True`` (the default here) tells the analyzer to skip
        the slow madmom downbeat pass — the library panel only needs
        BPM / key / duration to render rows; downbeats can be filled
        in lazily on first DeckLoad.
        """
        directory_p = Path(directory).expanduser()
        if not directory_p.exists() or not directory_p.is_dir():
            raise NotADirectoryError(f"not a directory: {directory_p}")

        # Snapshot existing IDs so the scan is idempotent (re-running
        # over the same dir doesn't re-analyze every file).
        existing = {
            row["track_id"]
            for row in self._conn.execute("SELECT track_id FROM tracks")
        }

        added: list[TrackRef] = []
        for path in sorted(directory_p.rglob("*")):
            if not path.is_file():
                continue
            if path.suffix.lower() not in exts:
                continue
            if path.stem in existing:
                continue
            try:
                ref = self.add_track_from_path(path, quick=quick)
            except Exception:  # noqa: BLE001 — analyzer is best-effort
                # One bad file shouldn't abort the whole scan; skip and continue.
                continue
            added.append(ref)
        return added

    def pick_compatible_for(
        self,
        playing_bpm: float,
        playing_camelot: str,
        exclude_ids: set[str] | None = None,
        max_bpm_stretch: float = _MAX_BPM_STRETCH,
        max_key_distance: int = _MAX_KEY_DISTANCE,
        top_k: int = 20,
    ) -> list[TrackRef]:
        """Return candidates passing the BPM + key gates, ordered by combined
        compatibility (closer BPM + closer key first).

        Tracks listed in ``exclude_ids`` (e.g. the currently playing track)
        are filtered out. Caller decides the final pick via mashability score
        — this is a *gate*, not a ranker, but we pre-sort so the top of the
        list is the obvious-best candidate.
        """
        exclude = exclude_ids or set()
        # SQL pre-filter on BPM window; key filter happens in Python because
        # Camelot adjacency isn't expressible cleanly in SQL.
        bpm_lo = playing_bpm * (1 - max_bpm_stretch)
        bpm_hi = playing_bpm * (1 + max_bpm_stretch)
        rows = self._conn.execute(
            "SELECT * FROM tracks WHERE bpm BETWEEN ? AND ? ORDER BY bpm",
            (bpm_lo, bpm_hi),
        ).fetchall()
        candidates: list[tuple[float, TrackRef]] = []
        for r in rows:
            if r["track_id"] in exclude:
                continue
            key_d = camelot_distance(playing_camelot, r["camelot_key"])
            if key_d > max_key_distance:
                continue
            ref = self._row_to_ref(r)
            bpm_d = bpm_stretch_ratio(playing_bpm, ref.bpm)
            # Sort score: weight key harder than BPM (1 step on the wheel
            # is more painful than 5% stretch).
            sort_score = key_d * 2.0 + bpm_d * 10.0
            candidates.append((sort_score, ref))
        candidates.sort(key=lambda x: x[0])
        return [c[1] for c in candidates[:top_k]]

    def __repr__(self) -> str:  # pragma: no cover — debugging only
        return f"TrackLibrary(db_path={self._db_path!r})"

    @staticmethod
    def _row_to_ref(r: sqlite3.Row) -> TrackRef:
        # New columns are NOT NULL DEFAULT so they're always present, but
        # we tolerate row shapes lacking them (e.g. an older driver path)
        # by falling back to TrackRef defaults.
        keys = set(r.keys())
        downbeats_raw = r["downbeats_json"] if "downbeats_json" in keys else "[]"
        try:
            downbeats_ms = [int(x) for x in json.loads(downbeats_raw or "[]")]
        except (ValueError, TypeError, json.JSONDecodeError):
            # Defensive: a corrupted downbeats_json shouldn't make the
            # whole row unreadable. Log path is the caller's concern.
            downbeats_ms = []
        bpm = float(r["bpm"])
        beat_period_default = 60_000.0 / bpm if bpm > 0 else 500.0
        return TrackRef(
            track_id=r["track_id"],
            path=r["path"],
            bpm=bpm,
            camelot_key=r["camelot_key"],
            energy=float(r["energy"]),
            duration_s=float(r["duration_s"]),
            beat_grid_anchor_ms=(
                int(r["beat_grid_anchor_ms"])
                if "beat_grid_anchor_ms" in keys
                else 0
            ),
            beat_period_ms=(
                float(r["beat_period_ms"])
                if "beat_period_ms" in keys
                else beat_period_default
            ),
            downbeats_ms=downbeats_ms,
        )


def _cli(argv: list[str] | None = None) -> int:
    """Tiny argparse shim — ``python -m copilot.library add <dir>``.

    Surfaced by the UI's library empty-state. Keeps the heavy analyzer
    import lazy via :meth:`TrackLibrary.add_tracks_from_directory`.
    """
    import argparse
    import os
    import sys

    p = argparse.ArgumentParser(prog="python -m copilot.library")
    p.add_argument(
        "--db",
        default=os.environ.get(
            "HYPEHOUSE_LIBRARY_DB", str(Path("~/.hypehouse-live/library.db"))
        ),
        help="SQLite library path (default ~/.hypehouse-live/library.db).",
    )
    sub = p.add_subparsers(dest="cmd", required=True)
    add = sub.add_parser("add", help="recursively analyze a directory of audio")
    add.add_argument("path", help="directory to scan")
    add.add_argument(
        "--quick",
        action="store_true",
        help="skip the slow downbeat pass (UI-only metadata still works)",
    )
    args = p.parse_args(argv if argv is not None else sys.argv[1:])

    lib = TrackLibrary(args.db)
    try:
        if args.cmd == "add":
            added = lib.add_tracks_from_directory(args.path, quick=args.quick)
            print(f"added {len(added)} new track(s) to {args.db}")
            return 0
        return 2  # unreachable — argparse enforces required=True
    finally:
        lib.close()


if __name__ == "__main__":  # pragma: no cover — CLI entry point
    import sys
    sys.exit(_cli())
