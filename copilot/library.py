"""Track library — read-only SQLite-backed catalog of analyzed tracks.

The library stores the **already-analyzed** track metadata (BPM, Camelot key,
energy, duration). Analysis itself happens out-of-band, either via HypeHouse
v1's CLI or via a future ingestion job — the co-pilot never analyzes inline
because librosa.load can deadlock on cold-container starts (see HypeHouse v1
postmortem on Cloud Run audio decode).

Schema::

    CREATE TABLE tracks (
        track_id     TEXT PRIMARY KEY,
        path         TEXT NOT NULL,
        bpm          REAL NOT NULL,
        camelot_key  TEXT NOT NULL,
        energy       REAL NOT NULL,
        duration_s   REAL NOT NULL
    );

The default DB lives at ``~/.hypehouse-live/library.db``; tests inject a
``:memory:`` DB via the ``db_path`` constructor argument.
"""
from __future__ import annotations

import sqlite3
from dataclasses import dataclass
from pathlib import Path

# Camelot wheel ordering — same convention as HypeHouse v1 analyzer.py.
# Index = (number-1) * 2 + (0 if A else 1). Used for circular distance only.
_CAMELOT_NUMBERS = list(range(1, 13))  # 1..12


@dataclass(frozen=True)
class TrackRef:
    """A library entry, shape-compatible with what the engine expects."""

    track_id: str
    path: str
    bpm: float
    camelot_key: str  # e.g. "8B", "10A"
    energy: float  # 0..~1, RMS of the analyzed window
    duration_s: float


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
            CREATE INDEX IF NOT EXISTS tracks_bpm_idx ON tracks (bpm);
            CREATE INDEX IF NOT EXISTS tracks_key_idx ON tracks (camelot_key);
            """
        )
        self._conn.commit()

    def close(self) -> None:
        self._conn.close()

    # --- write path (ingestion / tests) -----------------------------------

    def add_track(self, track: TrackRef) -> None:
        self._conn.execute(
            "INSERT OR REPLACE INTO tracks "
            "(track_id, path, bpm, camelot_key, energy, duration_s) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            (
                track.track_id,
                track.path,
                float(track.bpm),
                track.camelot_key,
                float(track.energy),
                float(track.duration_s),
            ),
        )
        self._conn.commit()

    # --- read path --------------------------------------------------------

    def all_tracks(self) -> list[TrackRef]:
        return [self._row_to_ref(r) for r in self._conn.execute("SELECT * FROM tracks")]

    def get(self, track_id: str) -> TrackRef | None:
        r = self._conn.execute(
            "SELECT * FROM tracks WHERE track_id = ?", (track_id,)
        ).fetchone()
        return self._row_to_ref(r) if r else None

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

    @staticmethod
    def _row_to_ref(r: sqlite3.Row) -> TrackRef:
        return TrackRef(
            track_id=r["track_id"],
            path=r["path"],
            bpm=float(r["bpm"]),
            camelot_key=r["camelot_key"],
            energy=float(r["energy"]),
            duration_s=float(r["duration_s"]),
        )
