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
from typing import TYPE_CHECKING

if TYPE_CHECKING:  # pragma: no cover — type-only import to break cycle.
    from .presets import PresetStore

# Bumped to v5 in the stem-separation scaffold PR.
# v2 added beat-grid columns (anchor_ms / period_ms / downbeats_json);
# v3 added ``hot_cues_json`` so the 8-slot per-track hot-cue grid
# survives track unload/reload;
# v4 added ``waveform_peaks`` — a packed min/max peak-pairs BLOB used
# by the UI to draw a real waveform instead of a flat placeholder line;
# v5 adds ``stems_dir`` + ``stems_status`` — filesystem path to the
# per-track 4-stem cache (vocals/drums/bass/other WAVs) and the
# computation status string ("pending" / "ready" / "failed"). NULL on
# both columns = stems have never been requested for this track. See
# ``copilot/stems.py`` for the on-disk layout.
# v6 adds the ``presets`` table for user preset snapshots (per-deck
# effects + EQ + pitch/tempo + crossfader curve). Storage scheme is a
# JSON blob in a single ``json`` column — schema-light because the
# captured field set is still churning. See ``copilot/presets.py`` for
# the in-memory shape + wire projection.
# v8 adds the ``playlist_queue`` table — DJ-curated next-track order
# consulted by the auto-mix controller before the mashability ranker.
# See ``copilot/playlist.py`` for the row shape, mutation API, and the
# auto-mix integration contract.
# Migrations dispatch on the gap between this constant and the value
# recorded in the ``schema_version`` table.
TRACK_SCHEMA_VERSION = 8

LOUDNESS_TARGET_LUFS = -14.0

# Per-track stem-status enum values. Persisted as plain strings (not a
# SQL enum) so older readers can still introspect the column. The
# pipeline never writes anything outside this set.
STEMS_STATUS_PENDING = "pending"
STEMS_STATUS_READY = "ready"
STEMS_STATUS_FAILED = "failed"

# Number of hot-cue slots per deck — mirrors the engine's
# ``Deck::hot_cues: [Option<u64>; 8]`` array in ``engine/src/state.rs``.
# Keeping the constant here means the library + RPC + tests don't drift
# from the engine when slot count changes (a remote possibility but the
# wire shape would break loudly).
HOT_CUE_SLOTS = 8

# DB-side default for the ``hot_cues_json`` column — a JSON array of 8
# nulls representing "no slot is set". Stored as a SQL literal so a
# pre-existing row migrated in via ``ALTER TABLE ADD COLUMN`` gets the
# correct backfill without a row-level UPDATE pass.
_HOT_CUES_EMPTY_JSON = "[null,null,null,null,null,null,null,null]"

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
    # 8-slot hot-cue grid. Each slot stores either a track-relative ms
    # position (``int >= 0``) or ``None`` for an empty slot. Mirrors the
    # engine's ``Deck::hot_cues: [Option<u64>; 8]`` so a row can be
    # passed straight into a ``DeckLoad`` event extension. Defaulting
    # to a fresh list of 8 ``None``s keeps existing positional call
    # sites (TrackRef(id, path, bpm, key, energy, dur)) working —
    # they get an empty hot-cue grid for free.
    hot_cues: list[int | None] = field(
        default_factory=lambda: [None] * HOT_CUE_SLOTS
    )
    # Loudness leveler fields (schema v7). NULL for tracks that
    # pre-date v7 or were added via the test ``add_track`` path
    # without a loudness measurement. ``lufs`` = raw ITU-R BS.1770
    # integrated loudness; ``track_gain_db`` = pre-computed gain to
    # land at :data:`LOUDNESS_TARGET_LUFS` (-14). Engine reads None
    # as 0 dB (passthrough).
    lufs: float | None = None
    track_gain_db: float | None = None


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
        if "hot_cues_json" not in cols:
            # v3 migration — adds the 8-slot hot-cue grid persistence
            # column. Existing rows are backfilled by SQLite with the
            # DEFAULT literal (8 nulls), no UPDATE needed. The column
            # is NOT NULL so a buggy writer can't insert a row without
            # an explicit cue array — fail loud at insert time rather
            # than fall back to a silent ``"[]"`` on read.
            self._conn.execute(
                "ALTER TABLE tracks "
                f"ADD COLUMN hot_cues_json TEXT NOT NULL DEFAULT '{_HOT_CUES_EMPTY_JSON}'"
            )
        if "waveform_peaks" not in cols:
            # v4 migration — adds the packed peak-pairs BLOB column.
            # NULL = "not yet computed"; the RPC handler treats NULL
            # as an explicit "compute on demand" signal so existing
            # rows don't need a backfill pass on schema upgrade.
            # Format is documented in ``copilot.waveform.compute_peaks``
            # (2*N i8 bytes, min/max per bucket).
            self._conn.execute(
                "ALTER TABLE tracks ADD COLUMN waveform_peaks BLOB"
            )
        if "stems_dir" not in cols:
            # v5 migration — filesystem path to the per-track stem
            # cache directory (``vocals.wav`` / ``drums.wav`` /
            # ``bass.wav`` / ``other.wav``). NULL = stems have never
            # been requested. Stored as a string rather than computed
            # at read time so a future change to the cache-root layout
            # doesn't strand existing rows.
            self._conn.execute(
                "ALTER TABLE tracks ADD COLUMN stems_dir TEXT"
            )
        if "stems_status" not in cols:
            # v5 migration — computation status. NULL = never
            # requested; "pending" = computation kicked off; "ready" =
            # all four WAVs on disk; "failed" = demucs raised (the
            # error message is in the engine log, not persisted here —
            # we'd need a status_detail column for that, deferred).
            self._conn.execute(
                "ALTER TABLE tracks ADD COLUMN stems_status TEXT"
            )
        if "lufs" not in cols:
            # v7 migration — integrated loudness (ITU-R BS.1770).
            # NULLABLE: older rows that pre-date the loudness pass
            # don't get retro-analyzed on first open (would force a
            # librosa cold-start tax + a re-decode of every file in
            # the catalog). The engine reads NULL as "no measurement",
            # which it translates to 0 dB gain — i.e. unchanged from
            # the pre-loudness-PR behaviour.
            self._conn.execute(
                "ALTER TABLE tracks ADD COLUMN lufs REAL"
            )
        if "track_gain_db" not in cols:
            # v7 migration — pre-computed engine-side gain to land at
            # the -14 LUFS streaming reference. Stored alongside the
            # raw ``lufs`` so the DeckLoad payload is a single REAL
            # column read; same NULL semantics as ``lufs``.
            self._conn.execute(
                "ALTER TABLE tracks ADD COLUMN track_gain_db REAL"
            )
        # v6 migration — user preset snapshots. Lives in its own table
        # rather than as columns on ``tracks`` because presets aren't
        # per-track. ``name`` is UNIQUE so the UI's "save current" flow
        # can detect duplicate names and prompt the user to rename
        # rather than silently overwrite. ``json`` holds the full
        # preset body so we don't need an ALTER TABLE per new field
        # (effect set + EQ + pitch/tempo + curve are likely to grow).
        self._conn.executescript(
            """
            CREATE TABLE IF NOT EXISTS presets (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                name       TEXT NOT NULL UNIQUE,
                json       TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS presets_created_at_idx
                ON presets (created_at DESC);
            """
        )
        # v8 migration — DJ playlist queue. One row per queued track;
        # ``position`` is the 0-indexed play order (dense; recomputed
        # on every mutation by :class:`copilot.playlist.PlaylistQueue`
        # so the wire shape never carries holes). ``added_at`` is a
        # plain ISO-8601 string so debug dumps stay grep-friendly —
        # SQLite has no native TIMESTAMP type. ``track_id`` is NOT a
        # SQL FK against ``tracks`` because a library re-scan can
        # legitimately drop rows that the queue still references; the
        # dequeue path resolves liveness against the library at
        # consume time and silently skips dangling entries.
        self._conn.executescript(
            """
            CREATE TABLE IF NOT EXISTS playlist_queue (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                track_id   TEXT NOT NULL,
                position   INTEGER NOT NULL,
                added_at   TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS playlist_queue_pos_idx
                ON playlist_queue (position);
            """
        )
        # Stamp the current schema version. The ``schema_version``
        # table is conceptually single-row but ``version`` is also the
        # primary key — using ``INSERT OR REPLACE`` would leave stale
        # rows behind on a v2→v3 migration (REPLACE matches on PK, so
        # a different version is a NEW row). DELETE-then-INSERT keeps
        # the table truly single-row across all migration paths.
        self._conn.execute("DELETE FROM schema_version")
        self._conn.execute(
            "INSERT INTO schema_version (version) VALUES (?)",
            (TRACK_SCHEMA_VERSION,),
        )
        self._conn.commit()

    def close(self) -> None:
        self._conn.close()

    def preset_store(self) -> "PresetStore":
        """Return a :class:`copilot.presets.PresetStore` over the same DB.

        Lazy-imported to keep :mod:`copilot.library` importable without
        pulling in the preset module — symmetric with the lazy demucs
        import on stems. The store shares this library's connection, so
        a preset save and a track read both hit the same SQLite file.
        """
        from .presets import PresetStore

        return PresetStore(self._conn)

    # --- write path (ingestion / tests) -----------------------------------

    def add_track(self, track: TrackRef) -> None:
        # v7: ``lufs`` + ``track_gain_db`` are NULL-safe. A caller that
        # doesn't measure loudness (every existing test fixture) just
        # inserts NULL into both, which the engine reads as
        # passthrough.
        self._conn.execute(
            "INSERT OR REPLACE INTO tracks "
            "(track_id, path, bpm, camelot_key, energy, duration_s, "
            " beat_grid_anchor_ms, beat_period_ms, downbeats_json, "
            " hot_cues_json, lufs, track_gain_db) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                _hot_cues_to_json(track.hot_cues),
                None if track.lufs is None else float(track.lufs),
                (
                    None
                    if track.track_gain_db is None
                    else float(track.track_gain_db)
                ),
            ),
        )
        self._conn.commit()

    def set_hot_cues(
        self, track_id: str, hot_cues: list[int | None]
    ) -> TrackRef:
        """Persist an updated hot-cue grid against an existing track.

        Validates the array shape (exactly :data:`HOT_CUE_SLOTS` slots,
        each ``None`` or a non-negative ``int``) and writes the JSON
        column atomically. Raises :class:`ValueError` on shape errors
        and :class:`KeyError` if ``track_id`` is not in the catalog —
        callers (the RPC handler) translate these into JSON-RPC error
        envelopes.
        """
        cues = _normalize_hot_cues(hot_cues)
        cursor = self._conn.execute(
            "UPDATE tracks SET hot_cues_json = ? WHERE track_id = ?",
            (_hot_cues_to_json(cues), track_id),
        )
        if cursor.rowcount == 0:
            # No row matched — surface so the caller can return a
            # JSON-RPC -32602 with a clear message rather than silently
            # no-op (which would mask a stale UI track id).
            raise KeyError(track_id)
        self._conn.commit()
        # Re-fetch to return the freshly persisted shape. Cheap (one
        # PK lookup) and avoids re-implementing the row->TrackRef map
        # at the caller.
        updated = self.get(track_id)
        assert updated is not None  # invariant: UPDATE just touched it
        return updated

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

        # Loudness pass (schema v7). Wrapped broad: a decode failure
        # here is best-effort metadata, not load-bearing for the
        # mashup gates. On failure both columns stay NULL and the
        # engine treats the deck as 0 dB gain — same as pre-PR. Lazy
        # import so a caller that never ingests audio doesn't pay
        # for pyloudnorm + librosa cold-starts.
        lufs_val: float | None = None
        gain_val: float | None = None
        try:
            from .loudness import compute_lufs, gain_db_for_target

            measured = compute_lufs(path_obj)
            lufs_val = float(measured)
            gain_val = float(gain_db_for_target(measured))
        except Exception:  # noqa: BLE001 — loudness is best-effort.
            pass

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
            lufs=lufs_val,
            track_gain_db=gain_val,
        )
        self.add_track(ref)
        # Compute + persist visual peak-pairs for the UI's waveform
        # canvas. Wrapped in a broad except because a decode failure
        # here mustn't abort track ingestion — BPM / key are the
        # mashing-critical fields, peaks are a render-time nicety.
        # The RPC handler's NULL-peaks branch handles the fallback.
        try:
            from .waveform import compute_peaks  # local import — librosa cold-start.

            peaks = compute_peaks(path_obj)
            self.set_waveform(ref.track_id, peaks)
        except Exception:  # noqa: BLE001 — peaks are best-effort.
            # Leave waveform_peaks NULL; UI falls back to flat line.
            pass
        return ref

    # --- read path --------------------------------------------------------

    def all_tracks(self) -> list[TrackRef]:
        return [self._row_to_ref(r) for r in self._conn.execute("SELECT * FROM tracks")]

    def get(self, track_id: str) -> TrackRef | None:
        r = self._conn.execute(
            "SELECT * FROM tracks WHERE track_id = ?", (track_id,)
        ).fetchone()
        return self._row_to_ref(r) if r else None

    def get_waveform(self, track_id: str) -> bytes | None:
        """Return the stored peak-pairs BLOB for a track, or ``None``.

        Returns ``None`` when:

        * ``track_id`` doesn't exist in the catalog, OR
        * the row exists but ``waveform_peaks`` is still NULL (the
          analyzer hasn't filled it yet — pre-v4 row that wasn't
          re-analyzed, or a stub ``add_track`` insert from a test).

        Callers (the RPC handler) translate both cases into the same
        ``{peaks_b64: null}`` wire shape so the UI's "no peaks ⇒ draw
        flat line" branch covers them both. Lazy-compute on first
        request lives at the RPC layer, not here — keeping this method
        a pure read keeps it cheap to call from tests + tight loops.
        """
        r = self._conn.execute(
            "SELECT waveform_peaks FROM tracks WHERE track_id = ?",
            (track_id,),
        ).fetchone()
        if r is None:
            return None
        blob = r["waveform_peaks"]
        if blob is None:
            return None
        return bytes(blob)

    def set_waveform(self, track_id: str, peaks: bytes) -> None:
        """Persist a peak-pairs BLOB against an existing track.

        Used by the analyzer (during ``add_track_from_path``) and by
        the RPC handler's lazy-compute path. Raises :class:`KeyError`
        if ``track_id`` doesn't exist — surfaced so a stale UI track
        id doesn't silently no-op into a missing waveform.
        """
        cursor = self._conn.execute(
            "UPDATE tracks SET waveform_peaks = ? WHERE track_id = ?",
            (peaks, track_id),
        )
        if cursor.rowcount == 0:
            raise KeyError(track_id)
        self._conn.commit()

    # --- stems persistence (v5) ------------------------------------------

    def get_stems_status(
        self, track_id: str
    ) -> tuple[str | None, str | None] | None:
        """Return ``(status, stems_dir)`` for ``track_id`` or ``None``.

        ``None`` is returned when the track row doesn't exist. When the
        row exists but stems haven't been computed, ``status`` and
        ``stems_dir`` are both ``None`` — caller distinguishes
        "unknown track" from "stems not requested yet" via this two-tier
        signal.
        """
        r = self._conn.execute(
            "SELECT stems_status, stems_dir FROM tracks WHERE track_id = ?",
            (track_id,),
        ).fetchone()
        if r is None:
            return None
        status = r["stems_status"]
        stems_dir = r["stems_dir"]
        return (status, stems_dir)

    def set_stems(
        self,
        track_id: str,
        *,
        status: str,
        stems_dir: str | Path | None,
    ) -> None:
        """Persist stem-cache metadata against an existing track row.

        Args:
            track_id: Library row id.
            status: One of ``"pending"`` / ``"ready"`` / ``"failed"`` —
                or any custom string; validation belongs at the caller
                (we don't enum here because adding a status string
                later shouldn't break old DB writers).
            stems_dir: Absolute filesystem path to the per-track
                cache. ``None`` is allowed (e.g. when status flips to
                ``"failed"`` before a dir was even created).

        Raises:
            KeyError: ``track_id`` not in the catalog.
        """
        path_str: str | None = (
            str(stems_dir) if stems_dir is not None else None
        )
        cursor = self._conn.execute(
            "UPDATE tracks SET stems_status = ?, stems_dir = ? "
            "WHERE track_id = ?",
            (status, path_str, track_id),
        )
        if cursor.rowcount == 0:
            raise KeyError(track_id)
        self._conn.commit()

    def compute_track_stems(
        self,
        track_id: str,
        output_root: str | Path | None = None,
    ) -> dict[str, Path]:
        """Run stem separation for ``track_id`` and persist the result.

        Looks up the track, resolves the per-track cache directory
        under ``output_root`` (defaults to
        :func:`copilot.stems.default_stems_root`), then calls
        :func:`copilot.stems.compute_stems`. Status is flipped to
        ``"pending"`` before the heavy call and to ``"ready"`` /
        ``"failed"`` after.

        Returns the same dict :func:`copilot.stems.compute_stems`
        returns: ``{"vocals": <Path>, ...}``.

        Raises:
            KeyError: ``track_id`` not in the catalog.
            StemsDependencyError: demucs not installed (the caller —
                typically the RPC layer — surfaces this as a
                "pip install [stems]" hint).
            Exception: any other failure from demucs propagates; the
                ``stems_status`` row is updated to ``"failed"`` first
                so a follow-up ``library.get_stems`` reports the
                failure cleanly.
        """
        # Lazy import to keep ``copilot.library`` itself free of the
        # demucs dependency at import time (and to let tests monkey-
        # patch ``copilot.stems.compute_stems`` without hitting an
        # ``ImportError`` from the top of this module).
        from . import stems as stems_mod

        ref = self.get(track_id)
        if ref is None:
            raise KeyError(track_id)

        root = (
            Path(output_root).expanduser()
            if output_root is not None
            else stems_mod.default_stems_root()
        )
        track_dir = root / track_id

        self.set_stems(
            track_id,
            status=STEMS_STATUS_PENDING,
            stems_dir=str(track_dir),
        )
        try:
            result = stems_mod.compute_stems(Path(ref.path), track_dir)
        except Exception:
            # Persist failure state and re-raise — the RPC layer wants
            # to see the exception type (specifically StemsDependency-
            # Error gets a different wire code).
            self.set_stems(
                track_id,
                status=STEMS_STATUS_FAILED,
                stems_dir=str(track_dir),
            )
            raise
        self.set_stems(
            track_id,
            status=STEMS_STATUS_READY,
            stems_dir=str(track_dir),
        )
        return result

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
        bpm_min: float | None = None,
        bpm_max: float | None = None,
        compatible_with_track_id: str | None = None,
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

        Structured filter args (the smart-filters UI surface — these
        compose with the inline shorthand above so a chip + a typed
        ``bpm:`` token AND together rather than fighting):

        * ``bpm_min`` / ``bpm_max`` — inclusive BPM range, applied as
          an extra ``WHERE bpm BETWEEN`` clause. Either bound can be
          ``None`` to leave that side open.
        * ``compatible_with_track_id`` — load the reference track's
          Camelot key, then post-filter candidates by
          ``camelot_distance ≤ 2`` against it. Distance threshold of
          ``2`` matches :data:`_MAX_KEY_DISTANCE` — same / adjacent /
          relative / fifth-circle — exactly the harmonic envelope the
          mashup ranker already considers compatible. Tracks with an
          unknown / malformed key (returns 99 from
          :func:`camelot_distance`) are filtered out automatically.
          Reference track itself is excluded from results so a user
          looking for "what mixes into this" doesn't see it back.
          If the reference id doesn't exist in the catalog, the
          filter degrades to a no-op (empty key match means no rows
          pass the post-filter) — caller responsibility to validate
          the id; we don't raise.
        """
        tokens = (query or "").strip().split()

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

        # Structured BPM range from the chip UI. Composes with any
        # inline ``bpm:`` token via an additional AND clause — the
        # narrowest of the two wins, which matches user expectation
        # ("chip says 120-130, typed bpm:124-126" -> 124-126).
        if bpm_min is not None:
            clauses.append("bpm >= ?")
            params.append(float(bpm_min))
        if bpm_max is not None:
            clauses.append("bpm <= ?")
            params.append(float(bpm_max))

        clamped_limit = max(1, min(int(limit), 1000))

        # Empty query AND no structured filters AND no harmonic
        # filter -> degrade to the same fast path as ``list_tracks``
        # so we don't pay for a degenerate SELECT * WHERE TRUE.
        if not clauses and compatible_with_track_id is None:
            return self.list_tracks(limit=clamped_limit, offset=0)

        if clauses:
            where = " AND ".join(clauses)
            sql = (
                "SELECT * FROM tracks "
                f"WHERE {where} "
                "ORDER BY track_id LIMIT ?"
            )
        else:
            # ``compatible_with`` is the only filter — we still need
            # to scan the catalog to evaluate camelot_distance in
            # Python. Cap the pre-filter at the larger of `limit` and
            # a conservative 1000 so post-filter still has room to
            # find compatible tracks even when the limit is small.
            sql = (
                "SELECT * FROM tracks "
                "ORDER BY track_id LIMIT ?"
            )
        # We over-fetch when a Python-side compat filter will trim
        # rows after the fact — otherwise the SQL LIMIT would silently
        # eat compatible candidates. 1000 matches the upstream clamp
        # in ``list_tracks`` so a "show me all compatible tracks"
        # filter sweeps a reasonable corpus without unbounded scans.
        sql_limit = (
            max(clamped_limit, 1000)
            if compatible_with_track_id is not None
            else clamped_limit
        )
        params.append(sql_limit)
        rows = self._conn.execute(sql, params).fetchall()
        results = [self._row_to_ref(r) for r in rows]

        if compatible_with_track_id is not None:
            ref = self.get(compatible_with_track_id)
            ref_key = ref.camelot_key if ref is not None else ""
            # Empty reference key (or unknown id) -> no candidate
            # passes the distance threshold (camelot_distance returns
            # 99 for "" vs anything), which is the same shape as a
            # truly empty match. UI surfaces "no matches".
            results = [
                t
                for t in results
                if t.track_id != compatible_with_track_id
                and camelot_distance(ref_key, t.camelot_key) <= _MAX_KEY_DISTANCE
            ]
            results = results[:clamped_limit]

        return results

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
        hot_cues_raw = (
            r["hot_cues_json"] if "hot_cues_json" in keys else _HOT_CUES_EMPTY_JSON
        )
        hot_cues = _hot_cues_from_json(hot_cues_raw)
        bpm = float(r["bpm"])
        beat_period_default = 60_000.0 / bpm if bpm > 0 else 500.0
        # v7 loudness columns — NULL-safe.
        lufs_raw = r["lufs"] if "lufs" in keys else None
        gain_raw = r["track_gain_db"] if "track_gain_db" in keys else None
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
            hot_cues=hot_cues,
            lufs=None if lufs_raw is None else float(lufs_raw),
            track_gain_db=None if gain_raw is None else float(gain_raw),
        )


# --- hot-cue helpers (module-level so tests + RPC can import) -------


def _normalize_hot_cues(hot_cues: list[int | None]) -> list[int | None]:
    """Validate + normalize a caller-supplied 8-slot hot-cue array.

    Returns a fresh list (caller-mutation safety). Raises
    :class:`ValueError` on:

    * wrong length (must be exactly :data:`HOT_CUE_SLOTS`),
    * a slot value that isn't ``None`` / ``int``,
    * a negative slot value (positions are non-negative).

    Bools are rejected explicitly because Python treats ``bool`` as a
    subclass of ``int`` — without the check, ``True`` would silently
    become ``1`` ms and ``False`` would become ``0`` ms.
    """
    if not isinstance(hot_cues, list):
        raise ValueError(
            f"hot_cues must be a list, got {type(hot_cues).__name__}"
        )
    if len(hot_cues) != HOT_CUE_SLOTS:
        raise ValueError(
            f"hot_cues must have exactly {HOT_CUE_SLOTS} slots, "
            f"got {len(hot_cues)}"
        )
    out: list[int | None] = []
    for i, v in enumerate(hot_cues):
        if v is None:
            out.append(None)
            continue
        if isinstance(v, bool) or not isinstance(v, int):
            raise ValueError(
                f"hot_cues[{i}] must be int or None, "
                f"got {type(v).__name__}"
            )
        if v < 0:
            raise ValueError(
                f"hot_cues[{i}] must be non-negative, got {v}"
            )
        out.append(int(v))
    return out


def _hot_cues_to_json(hot_cues: list[int | None]) -> str:
    """Serialize a hot-cue array to JSON for SQLite storage.

    Skips re-validation in the hot path — :func:`_normalize_hot_cues`
    runs at the boundary (``set_hot_cues`` / ``add_track`` callers).
    """
    return json.dumps(list(hot_cues))


def _hot_cues_from_json(raw: str | None) -> list[int | None]:
    """Parse the ``hot_cues_json`` column into the in-memory shape.

    Returns a fresh 8-slot list of ``None``s when the JSON is missing
    or malformed — a corrupted cell shouldn't propagate as a row-read
    error. Length mismatches (e.g. an old column with fewer slots) are
    padded/truncated to :data:`HOT_CUE_SLOTS` so the engine's fixed
    array always lines up.
    """
    if not raw:
        return [None] * HOT_CUE_SLOTS
    try:
        parsed = json.loads(raw)
    except (ValueError, TypeError, json.JSONDecodeError):
        return [None] * HOT_CUE_SLOTS
    if not isinstance(parsed, list):
        return [None] * HOT_CUE_SLOTS
    out: list[int | None] = []
    for v in parsed[:HOT_CUE_SLOTS]:
        if v is None:
            out.append(None)
        elif isinstance(v, bool):  # reject bool early — see _normalize.
            out.append(None)
        elif isinstance(v, int) and v >= 0:
            out.append(v)
        else:
            out.append(None)
    while len(out) < HOT_CUE_SLOTS:
        out.append(None)
    return out


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
