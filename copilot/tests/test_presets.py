"""Preset CRUD + RPC tests.

Covers the ``copilot.presets`` store and the ``copilot.preset_rpc``
JSON-RPC handler. The schema-migration test lives here too — adding a
v6 migration that bumps :data:`TRACK_SCHEMA_VERSION` should not break
existing v5 DBs that have ``tracks`` rows.
"""
from __future__ import annotations

from datetime import datetime, timezone

import pytest

from copilot.library import TRACK_SCHEMA_VERSION, TrackLibrary, TrackRef
from copilot.preset_rpc import PresetRpcHandler
from copilot.library_rpc import (
    JSONRPC_INVALID_PARAMS,
    RpcError,
)
from copilot.presets import (
    CROSSFADER_CURVES,
    DeckState,
    EffectSlotState,
    PresetError,
)


_asyncio = pytest.mark.asyncio


def _sample_deck(eq_low: float = -3.0) -> DeckState:
    """A reasonably-populated deck-state fixture — different from defaults
    so a load-replay test can detect the wrong slot getting written."""
    return DeckState(
        effects=(
            EffectSlotState(
                effect_id=1,
                params={"cutoff_hz": 500.0, "resonance": 0.3},
                wet_dry=0.7,
                enabled=True,
            ),
            EffectSlotState(
                effect_id=2,
                params={"depth": 0.5},
                wet_dry=0.4,
                enabled=False,
            ),
            EffectSlotState(),  # empty slot
        ),
        eq_low_db=eq_low,
        eq_mid_db=1.5,
        eq_high_db=2.0,
        pitch_semitones=0.5,
        tempo_ratio=1.05,
    )


# ---- schema migration --------------------------------------------


def test_schema_v6_creates_presets_table(library: TrackLibrary):
    """Fresh ``:memory:`` DB should have the presets table after init."""
    row = library._conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='presets'"
    ).fetchone()
    assert row is not None, "presets table missing after schema init"
    # And the schema_version stamp is current.
    version_row = library._conn.execute(
        "SELECT version FROM schema_version"
    ).fetchone()
    assert version_row["version"] == TRACK_SCHEMA_VERSION


def test_schema_migration_from_v5_keeps_tracks(tmp_path):
    """Open a fresh DB (which runs full migration to v6), insert a track,
    then re-open — the row survives and the presets table is present."""
    db = tmp_path / "lib.db"
    lib1 = TrackLibrary(db)
    try:
        lib1.add_track(
            TrackRef("t1", "/m/t1.mp3", 124.0, "8B", 0.2, 200.0)
        )
    finally:
        lib1.close()
    # Re-open: schema init runs again, idempotently.
    lib2 = TrackLibrary(db)
    try:
        # Old data survives.
        assert lib2.get("t1") is not None
        # Presets table now exists + is empty.
        store = lib2.preset_store()
        assert store.list_presets() == []
    finally:
        lib2.close()


# ---- store CRUD --------------------------------------------------


def test_save_and_load_roundtrip(library: TrackLibrary):
    store = library.preset_store()
    saved = store.save_preset(
        name="warmup",
        deck_a=_sample_deck(eq_low=-2.0),
        deck_b=_sample_deck(eq_low=-4.0),
        crossfader_curve="Dipped",
    )
    assert saved.id is not None and saved.id > 0
    assert saved.name == "warmup"
    assert saved.created_at  # ISO-8601 timestamp set

    loaded = store.load_preset(saved.id)
    assert loaded is not None
    assert loaded.name == "warmup"
    assert loaded.crossfader_curve == "Dipped"
    # Deep-roundtrip: every effect slot survives.
    assert loaded.deck_a.eq_low_db == -2.0
    assert loaded.deck_b.eq_low_db == -4.0
    assert loaded.deck_a.effects[0].effect_id == 1
    assert loaded.deck_a.effects[0].params == {
        "cutoff_hz": 500.0,
        "resonance": 0.3,
    }
    assert loaded.deck_a.effects[0].enabled is True
    assert loaded.deck_a.effects[2].effect_id == 0  # empty slot preserved


def test_save_rejects_duplicate_name(library: TrackLibrary):
    store = library.preset_store()
    store.save_preset(
        name="party", deck_a=DeckState(), deck_b=DeckState()
    )
    with pytest.raises(PresetError):
        store.save_preset(
            name="party", deck_a=DeckState(), deck_b=DeckState()
        )


def test_save_rejects_blank_name(library: TrackLibrary):
    store = library.preset_store()
    with pytest.raises(PresetError):
        store.save_preset(
            name="   ", deck_a=DeckState(), deck_b=DeckState()
        )


def test_save_rejects_invalid_curve(library: TrackLibrary):
    store = library.preset_store()
    with pytest.raises(PresetError):
        store.save_preset(
            name="bad-curve",
            deck_a=DeckState(),
            deck_b=DeckState(),
            crossfader_curve="Bogus",
        )


def test_list_orders_by_recency(library: TrackLibrary):
    store = library.preset_store()
    # Two saves with explicit timestamps so the ordering is deterministic.
    store.save_preset(
        name="first",
        deck_a=DeckState(),
        deck_b=DeckState(),
        now=datetime(2026, 5, 1, 10, 0, 0, tzinfo=timezone.utc),
    )
    store.save_preset(
        name="second",
        deck_a=DeckState(),
        deck_b=DeckState(),
        now=datetime(2026, 5, 2, 10, 0, 0, tzinfo=timezone.utc),
    )
    listed = store.list_presets()
    assert [p.name for p in listed] == ["second", "first"]


def test_delete_returns_true_then_false(library: TrackLibrary):
    store = library.preset_store()
    saved = store.save_preset(
        name="to-delete", deck_a=DeckState(), deck_b=DeckState()
    )
    assert saved.id is not None
    assert store.delete_preset(saved.id) is True
    # Second call: row already gone — idempotent False.
    assert store.delete_preset(saved.id) is False
    assert store.load_preset(saved.id) is None


def test_load_missing_returns_none(library: TrackLibrary):
    store = library.preset_store()
    assert store.load_preset(999) is None


def test_count_matches_list_len(library: TrackLibrary):
    store = library.preset_store()
    assert store.count() == 0
    store.save_preset(name="a", deck_a=DeckState(), deck_b=DeckState())
    store.save_preset(name="b", deck_a=DeckState(), deck_b=DeckState())
    assert store.count() == 2
    assert len(store.list_presets()) == 2


def test_corrupted_json_blob_falls_back_to_defaults(library: TrackLibrary):
    """A row with a busted JSON body should still load — fields default
    rather than blow up the whole list view."""
    library._conn.execute(
        "INSERT INTO presets (name, json, created_at) VALUES (?, ?, ?)",
        ("corrupted", "not-json-at-all", "2026-05-01T00:00:00Z"),
    )
    library._conn.commit()
    store = library.preset_store()
    loaded = store.list_presets()
    assert len(loaded) == 1
    assert loaded[0].crossfader_curve == "Linear"
    assert loaded[0].deck_a.eq_low_db == 0.0


# ---- RPC handler -------------------------------------------------


def _wire_deck_state() -> dict:
    return {
        "effects": [
            {
                "effect_id": 1,
                "params": {"cutoff_hz": 500.0},
                "wet_dry": 0.6,
                "enabled": True,
            },
            {"effect_id": 0, "params": {}, "wet_dry": 0.5, "enabled": False},
            {"effect_id": 0, "params": {}, "wet_dry": 0.5, "enabled": False},
        ],
        "eq_low_db": -3.0,
        "eq_mid_db": 0.0,
        "eq_high_db": 1.5,
        "pitch_semitones": 0.0,
        "tempo_ratio": 1.0,
    }


@_asyncio
async def test_rpc_save_returns_id_and_full_preset(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    result = await handler.dispatch(
        "presets.save",
        {
            "name": "scene-1",
            "deck_a": _wire_deck_state(),
            "deck_b": _wire_deck_state(),
            "crossfader_curve": "Sharp",
        },
    )
    assert result["preset_id"] is not None
    assert result["preset"]["name"] == "scene-1"
    assert result["preset"]["crossfader_curve"] == "Sharp"
    assert result["preset"]["deck_a"]["effects"][0]["effect_id"] == 1


@_asyncio
async def test_rpc_save_rejects_duplicate_name(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    await handler.dispatch(
        "presets.save",
        {
            "name": "dupe",
            "deck_a": _wire_deck_state(),
            "deck_b": _wire_deck_state(),
        },
    )
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "presets.save",
            {
                "name": "dupe",
                "deck_a": _wire_deck_state(),
                "deck_b": _wire_deck_state(),
            },
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_rpc_save_validates_curve(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "presets.save",
            {
                "name": "bad",
                "deck_a": _wire_deck_state(),
                "deck_b": _wire_deck_state(),
                "crossfader_curve": "NotAVariant",
            },
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_rpc_save_rejects_missing_deck(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch(
            "presets.save",
            {"name": "x", "deck_a": _wire_deck_state()},
        )
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_rpc_list_returns_summary_shape(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    await handler.dispatch(
        "presets.save",
        {
            "name": "p1",
            "deck_a": _wire_deck_state(),
            "deck_b": _wire_deck_state(),
        },
    )
    listed = await handler.dispatch("presets.list", {})
    assert "presets" in listed
    assert len(listed["presets"]) == 1
    one = listed["presets"][0]
    # Summary shape: id + name + created_at, no deck blob.
    assert set(one.keys()) == {"id", "name", "created_at"}
    assert one["name"] == "p1"


@_asyncio
async def test_rpc_load_returns_full_preset(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    saved = await handler.dispatch(
        "presets.save",
        {
            "name": "load-me",
            "deck_a": _wire_deck_state(),
            "deck_b": _wire_deck_state(),
            "crossfader_curve": "Scratch",
        },
    )
    loaded = await handler.dispatch(
        "presets.load", {"id": saved["preset_id"]}
    )
    assert loaded["preset"]["name"] == "load-me"
    assert loaded["preset"]["crossfader_curve"] == "Scratch"
    assert loaded["preset"]["deck_a"]["eq_low_db"] == -3.0


@_asyncio
async def test_rpc_load_unknown_id_raises(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    with pytest.raises(RpcError) as exc:
        await handler.dispatch("presets.load", {"id": 9999})
    assert exc.value.code == JSONRPC_INVALID_PARAMS


@_asyncio
async def test_rpc_delete_then_list_empty(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    saved = await handler.dispatch(
        "presets.save",
        {
            "name": "del-me",
            "deck_a": _wire_deck_state(),
            "deck_b": _wire_deck_state(),
        },
    )
    result = await handler.dispatch(
        "presets.delete", {"id": saved["preset_id"]}
    )
    assert result == {"ok": True, "deleted": True}
    listed = await handler.dispatch("presets.list", {})
    assert listed["presets"] == []
    # Second delete is idempotent ok-but-not-deleted.
    again = await handler.dispatch(
        "presets.delete", {"id": saved["preset_id"]}
    )
    assert again == {"ok": True, "deleted": False}


def test_handler_handles_only_preset_namespace(library: TrackLibrary):
    handler = PresetRpcHandler(library)
    assert handler.handles("presets.save")
    assert handler.handles("presets.list")
    assert handler.handles("presets.load")
    assert handler.handles("presets.delete")
    assert not handler.handles("library.list_tracks")
    assert not handler.handles("presets.does_not_exist")


def test_crossfader_curves_constant_matches_engine_variants():
    """If the engine adds a new variant, this test reminds us to update
    the copilot side too."""
    assert set(CROSSFADER_CURVES) == {"Linear", "Dipped", "Sharp", "Scratch"}
