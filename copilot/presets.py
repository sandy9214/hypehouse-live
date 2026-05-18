"""User preset snapshots — save/recall a full deck-state scene.

A "preset" captures the live mixing state of both decks plus the
crossfader curve, so a DJ can stash a working scene and recall it later
in the night (or in a different session). The captured surface is the
*controllable* knobs only — playback position, BPM, loaded track, hot
cues, etc. are NOT part of a preset because they would either fight the
current playhead or paste in a stale track id. Snapshot scope:

* per-deck **3 effect slots** — `effect_id`, `params`, `wet_dry`,
  `enabled` (mirrors `EffectSlot` in `engine/src/state.rs`);
* per-deck **3-band EQ** — `eq_low_db`, `eq_mid_db`, `eq_high_db`;
* per-deck **pitch + tempo** — `pitch_semitones`, `tempo_ratio`;
* master **crossfader response curve** — `Linear`/`Dipped`/`Sharp`/`Scratch`.

Storage: a single `presets` table sharing the existing library SQLite
DB. Schema v6 adds the table on the existing migration ladder in
:mod:`copilot.library` so a fresh-DB and a v5-upgrade path converge on
the same shape. The full preset shape is stored as a JSON blob under
the `json` column — schema-light because the field set will churn over
the next few PRs (we'll add filter / sampler state once those land) and
ALTER TABLE per field is more friction than a JSON blob audit.

The UI's PresetPanel turns "load preset" into a sequence of
`submit_event` calls — one per slot per deck for effects, three EQ
bands per deck, pitch + tempo per deck, and one `SetCrossfaderCurve`.
The engine is event-sourced so the order doesn't matter for
correctness, only for the visible during-load animation.

This module is pure Python — no engine import, no audio side effects.
SQLite writes happen via a connection owned by the caller (typically
the shared :class:`copilot.library.TrackLibrary` handle) so a preset
save + a track read both hit the same DB file. See
:meth:`copilot.library.TrackLibrary.preset_store` for the wired factory.
"""
from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

# Number of effect slots per deck — mirrors engine `Deck::effects: [EffectSlot; 3]`.
EFFECT_SLOTS = 3

# Crossfader curve string set — mirrors the engine's `CrossfaderCurve`
# enum (engine/src/state.rs). Stored verbatim; reject anything else at
# the validation boundary so a corrupted preset doesn't reach the wire.
CROSSFADER_CURVES = ("Linear", "Dipped", "Sharp", "Scratch")


@dataclass(frozen=True)
class EffectSlotState:
    """Captured state of one effect slot.

    Mirrors :rust:`EffectSlot` in ``engine/src/state.rs`` so a preset
    can be replayed via the same `EffectAssign` / `EffectParam` /
    `EffectWetDry` / `EffectEnable` events the UI already emits.

    `effect_id == 0` is the "empty slot" sentinel — replay still emits
    an `EffectClear` for the slot so an empty preset slot wipes any
    prior assignment on load.
    """

    effect_id: int = 0
    params: dict[str, float] = field(default_factory=dict)
    wet_dry: float = 0.5
    enabled: bool = False


@dataclass(frozen=True)
class DeckState:
    """Captured controllable state of one deck.

    Excludes loaded track / position / hot cues by design — see module
    docstring. Adding a field here is a schema-light change because
    persistence is JSON; bump :data:`PRESET_JSON_VERSION` if a new
    field needs a migration on read.
    """

    effects: tuple[EffectSlotState, EffectSlotState, EffectSlotState] = field(
        default_factory=lambda: (
            EffectSlotState(),
            EffectSlotState(),
            EffectSlotState(),
        )
    )
    eq_low_db: float = 0.0
    eq_mid_db: float = 0.0
    eq_high_db: float = 0.0
    pitch_semitones: float = 0.0
    tempo_ratio: float = 1.0


@dataclass(frozen=True)
class Preset:
    """A complete scene snapshot.

    ``id`` is ``None`` for an unsaved preset and a positive int once
    persisted (SQLite ``INTEGER PRIMARY KEY``).

    ``created_at`` is set to the current UTC time at save time; tests
    that need a deterministic value pass it explicitly.
    """

    name: str
    deck_a: DeckState
    deck_b: DeckState
    crossfader_curve: str = "Linear"
    id: int | None = None
    created_at: str = ""


# ---- JSON (de)serialization ---------------------------------------


# Bump when the on-disk JSON shape changes in a non-backwards-compat
# way. We tag every blob so a future reader can dispatch on it cheaply
# without trying multiple decoders.
PRESET_JSON_VERSION = 1


def _effect_slot_to_dict(s: EffectSlotState) -> dict[str, Any]:
    return {
        "effect_id": int(s.effect_id),
        "params": {k: float(v) for k, v in s.params.items()},
        "wet_dry": float(s.wet_dry),
        "enabled": bool(s.enabled),
    }


def _deck_state_to_dict(d: DeckState) -> dict[str, Any]:
    return {
        "effects": [_effect_slot_to_dict(s) for s in d.effects],
        "eq_low_db": float(d.eq_low_db),
        "eq_mid_db": float(d.eq_mid_db),
        "eq_high_db": float(d.eq_high_db),
        "pitch_semitones": float(d.pitch_semitones),
        "tempo_ratio": float(d.tempo_ratio),
    }


def _preset_body_to_dict(p: Preset) -> dict[str, Any]:
    """JSON-blob projection of a preset (sans `id` / `name` / `created_at`).

    The `id` is the SQL PK, `name` is its own UNIQUE column, and
    `created_at` is its own column — keeping them out of the blob means
    we can rename a preset (or migrate `created_at` to a real timestamp
    column) without rewriting every JSON cell.
    """
    return {
        "version": PRESET_JSON_VERSION,
        "deck_a": _deck_state_to_dict(p.deck_a),
        "deck_b": _deck_state_to_dict(p.deck_b),
        "crossfader_curve": p.crossfader_curve,
    }


def _to_json(p: Preset) -> str:
    return json.dumps(_preset_body_to_dict(p), separators=(",", ":"))


def _coerce_float(v: Any, default: float) -> float:
    if isinstance(v, bool):  # bool is an int subclass — reject explicitly.
        return default
    if isinstance(v, (int, float)):
        return float(v)
    return default


def _coerce_effect_slot(raw: Any) -> EffectSlotState:
    if not isinstance(raw, dict):
        return EffectSlotState()
    params_raw = raw.get("params") or {}
    params: dict[str, float] = {}
    if isinstance(params_raw, dict):
        for k, v in params_raw.items():
            if isinstance(k, str):
                params[k] = _coerce_float(v, 0.0)
    return EffectSlotState(
        effect_id=int(raw.get("effect_id") or 0),
        params=params,
        wet_dry=_coerce_float(raw.get("wet_dry"), 0.5),
        enabled=bool(raw.get("enabled")),
    )


def _coerce_deck_state(raw: Any) -> DeckState:
    if not isinstance(raw, dict):
        return DeckState()
    effects_raw = raw.get("effects") or []
    slots: list[EffectSlotState] = []
    if isinstance(effects_raw, list):
        for i in range(EFFECT_SLOTS):
            slots.append(
                _coerce_effect_slot(effects_raw[i] if i < len(effects_raw) else None)
            )
    while len(slots) < EFFECT_SLOTS:
        slots.append(EffectSlotState())
    return DeckState(
        effects=(slots[0], slots[1], slots[2]),
        eq_low_db=_coerce_float(raw.get("eq_low_db"), 0.0),
        eq_mid_db=_coerce_float(raw.get("eq_mid_db"), 0.0),
        eq_high_db=_coerce_float(raw.get("eq_high_db"), 0.0),
        pitch_semitones=_coerce_float(raw.get("pitch_semitones"), 0.0),
        tempo_ratio=_coerce_float(raw.get("tempo_ratio"), 1.0),
    )


def _from_json(
    *,
    preset_id: int,
    name: str,
    created_at: str,
    body: str,
) -> Preset:
    """Decode one ``presets`` row into a :class:`Preset`.

    Tolerant by design — a row with a missing field falls back to
    defaults rather than raising, so a forward-compatible reader doesn't
    blow up on a preset saved by a future build with extra fields.
    """
    try:
        parsed = json.loads(body or "{}")
    except json.JSONDecodeError:
        parsed = {}
    if not isinstance(parsed, dict):
        parsed = {}
    curve = parsed.get("crossfader_curve") or "Linear"
    if curve not in CROSSFADER_CURVES:
        curve = "Linear"
    return Preset(
        id=preset_id,
        name=name,
        created_at=created_at,
        deck_a=_coerce_deck_state(parsed.get("deck_a")),
        deck_b=_coerce_deck_state(parsed.get("deck_b")),
        crossfader_curve=curve,
    )


# ---- input validation (RPC-friendly errors) -----------------------


class PresetError(ValueError):
    """Raised by :class:`PresetStore` on validation failures.

    Sub-classes nothing engine-specific so the RPC layer can translate
    one error type into a single ``-32602 invalid params`` envelope.
    """


def _validate_name(name: str) -> str:
    if not isinstance(name, str):
        raise PresetError("name must be a string")
    cleaned = name.strip()
    if not cleaned:
        raise PresetError("name must be non-empty")
    if len(cleaned) > 120:
        # Soft cap so the UI list doesn't blow up on a 5KB preset name.
        # The DB column has no length limit, so this is the only guard.
        raise PresetError("name must be 120 chars or fewer")
    return cleaned


def _validate_curve(curve: str) -> str:
    if curve not in CROSSFADER_CURVES:
        raise PresetError(
            f"crossfader_curve must be one of {CROSSFADER_CURVES}, got {curve!r}"
        )
    return curve


# ---- store --------------------------------------------------------


class PresetStore:
    """SQLite-backed CRUD for user preset snapshots.

    Wraps an existing :class:`sqlite3.Connection` so a single DB file
    can host both the track catalog and the preset table. The
    connection's schema migration runs once at
    :meth:`copilot.library.TrackLibrary._init_schema` — this class
    assumes the ``presets`` table is already present and does not
    re-issue ``CREATE TABLE``.

    All write methods commit synchronously — the surface is small
    enough that batching adds no value, and the RPC handler's
    transactional unit is "one save / one delete" anyway.
    """

    def __init__(self, conn: sqlite3.Connection):
        # Caller owns the connection's row_factory / lifecycle. We just
        # use it. row_factory is expected to be sqlite3.Row (set by
        # TrackLibrary); plain tuples would also work because we index
        # by column name via the helpers below.
        self._conn = conn

    # --- write path ---------------------------------------------------

    def save_preset(
        self,
        *,
        name: str,
        deck_a: DeckState,
        deck_b: DeckState,
        crossfader_curve: str = "Linear",
        now: datetime | None = None,
    ) -> Preset:
        """Persist a new preset.

        Returns the persisted :class:`Preset` (with assigned ``id`` and
        ``created_at``). Names must be unique — re-using a name raises
        :class:`PresetError` translated from the underlying
        :class:`sqlite3.IntegrityError`. Callers wanting "save or
        overwrite" semantics should delete-then-save.
        """
        clean_name = _validate_name(name)
        curve = _validate_curve(crossfader_curve)
        # ``now`` injection mostly exists so tests get deterministic
        # timestamps; the default uses UTC + ISO-8601 (Z-suffix) so the
        # wire shape is unambiguous when surfaced to a JS Date parser.
        ts = (now or datetime.now(timezone.utc)).strftime("%Y-%m-%dT%H:%M:%SZ")
        body = Preset(
            name=clean_name,
            deck_a=deck_a,
            deck_b=deck_b,
            crossfader_curve=curve,
            created_at=ts,
        )
        try:
            cursor = self._conn.execute(
                "INSERT INTO presets (name, json, created_at) VALUES (?, ?, ?)",
                (clean_name, _to_json(body), ts),
            )
        except sqlite3.IntegrityError as exc:
            # UNIQUE constraint on `name` is the only one in this table,
            # so any IntegrityError is a duplicate-name collision.
            raise PresetError(
                f"preset name already exists: {clean_name!r}"
            ) from exc
        self._conn.commit()
        return Preset(
            id=int(cursor.lastrowid or 0),
            name=clean_name,
            created_at=ts,
            deck_a=body.deck_a,
            deck_b=body.deck_b,
            crossfader_curve=body.crossfader_curve,
        )

    def delete_preset(self, preset_id: int) -> bool:
        """Delete a preset by id.

        Returns ``True`` if a row was removed, ``False`` if no preset
        matched (idempotent — repeated delete calls converge). The RPC
        layer surfaces ``False`` as ``ok: true`` because "preset
        already gone" is the user's terminal state either way.
        """
        cursor = self._conn.execute(
            "DELETE FROM presets WHERE id = ?", (int(preset_id),)
        )
        self._conn.commit()
        return cursor.rowcount > 0

    # --- read path ---------------------------------------------------

    def load_preset(self, preset_id: int) -> Preset | None:
        """Return the full preset shape, or ``None`` if no row matched."""
        row = self._conn.execute(
            "SELECT id, name, created_at, json FROM presets WHERE id = ?",
            (int(preset_id),),
        ).fetchone()
        if row is None:
            return None
        return _from_json(
            preset_id=int(row["id"]),
            name=str(row["name"]),
            created_at=str(row["created_at"] or ""),
            body=str(row["json"] or "{}"),
        )

    def list_presets(self) -> list[Preset]:
        """List all presets ordered by created_at DESC (most-recent first).

        Returns lightweight :class:`Preset` instances — the JSON blob is
        decoded so the caller can also use this for "load all" flows.
        The UI list view only needs `id` / `name` / `created_at`; the
        RPC wire shape (`presets.list`) drops the body to keep the list
        response small (see ``preset_rpc._preset_list_row``).
        """
        rows = self._conn.execute(
            "SELECT id, name, created_at, json FROM presets "
            "ORDER BY datetime(created_at) DESC, id DESC"
        ).fetchall()
        return [
            _from_json(
                preset_id=int(r["id"]),
                name=str(r["name"]),
                created_at=str(r["created_at"] or ""),
                body=str(r["json"] or "{}"),
            )
            for r in rows
        ]

    def count(self) -> int:
        """Total preset rows. Convenience for tests / future paginated UI."""
        r = self._conn.execute("SELECT COUNT(*) AS n FROM presets").fetchone()
        return int(r["n"]) if r else 0


# Module-level helpers exposed to callers / RPC layer.


def deck_state_from_wire(raw: Any) -> DeckState:
    """Construct a :class:`DeckState` from a wire dict.

    Validates types but is generous with missing fields — same shape
    rules as :func:`_coerce_deck_state` (which we just delegate to).
    Lifted to the module surface so the RPC handler doesn't reach
    into a leading-underscore name.
    """
    return _coerce_deck_state(raw)


def preset_to_wire(p: Preset) -> dict[str, Any]:
    """Project a :class:`Preset` into the wire dict the UI consumes."""
    return {
        "id": int(p.id) if p.id is not None else None,
        "name": p.name,
        "created_at": p.created_at,
        "deck_a": _deck_state_to_dict(p.deck_a),
        "deck_b": _deck_state_to_dict(p.deck_b),
        "crossfader_curve": p.crossfader_curve,
    }


def preset_summary_to_wire(p: Preset) -> dict[str, Any]:
    """Lightweight projection for `presets.list` — no deck blob."""
    return {
        "id": int(p.id) if p.id is not None else None,
        "name": p.name,
        "created_at": p.created_at,
    }
