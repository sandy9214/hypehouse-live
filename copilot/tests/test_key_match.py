"""Tests for :mod:`copilot.key_match` and the ``key_match.*`` RPC.

Coverage matrix:

* identical keys → 0.0
* parallel-fifth step (1 wheel step) → ±5 semitones after wrap
* relative minor/major (same number, different letter) → 0.0
* malformed / empty inputs → 0.0 (graceful)
* long-way-round wrap (5 steps clockwise = +35 → folded to -1)
* full octave (6 steps = +42 → folded to +6 exactly — boundary)
* RPC round-trip with two seeded tracks
* RPC error envelope when a track id is unknown
"""
from __future__ import annotations

import pytest

from copilot.key_match import camelot_to_semitones
from copilot.library import TrackLibrary, TrackRef
from copilot.library_rpc import JSONRPC_INVALID_PARAMS, LibraryRpcHandler, RpcError


# ---- camelot_to_semitones unit tests --------------------------------


def test_same_key_returns_zero() -> None:
    assert camelot_to_semitones("8B", "8B") == 0.0
    assert camelot_to_semitones("1A", "1A") == 0.0


def test_relative_minor_major_returns_zero() -> None:
    # 8B (C major) and 8A (A minor) share notes; pitching the audio
    # would actively un-match them. Same number + letter flip = 0.
    assert camelot_to_semitones("8B", "8A") == 0.0
    assert camelot_to_semitones("8A", "8B") == 0.0
    assert camelot_to_semitones("12B", "12A") == 0.0


def test_one_step_clockwise_is_minus_five_semitones() -> None:
    # 8B → 9B is +7 semitones up a perfect fifth. Folded into the
    # (-6, 6] window the shorter path is -5 (down a perfect fourth).
    # Both transpositions land on the same pitch class one octave apart.
    assert camelot_to_semitones("8B", "9B") == -5.0


def test_one_step_anti_clockwise_is_plus_five_semitones() -> None:
    # Going backward on the wheel (e.g. 9B → 8B) is -7 semitones raw,
    # which folds to +5 (up a perfect fourth) — again the shorter path.
    assert camelot_to_semitones("9B", "8B") == 5.0


def test_long_way_round_wraps_to_shorter_path() -> None:
    # 8B → 1B = 5 steps clockwise = +35 semitones raw → folded to -1.
    assert camelot_to_semitones("8B", "1B") == -1.0


def test_six_steps_is_tritone_boundary() -> None:
    # 6 steps = +42 semitones raw → folded to +6 (tritone). Boundary:
    # +6 stays positive, anything above flips negative.
    result = camelot_to_semitones("1B", "7B")
    assert result == 6.0
    # And the opposite direction also folds to +6 (tritone is
    # symmetric — there's no shorter path), not -6.
    assert camelot_to_semitones("7B", "1B") == 6.0


def test_malformed_or_empty_inputs_return_zero() -> None:
    # Defensive: missing key on either side → 0.0 so the UI's
    # graceful-degrade path works.
    assert camelot_to_semitones("", "8B") == 0.0
    assert camelot_to_semitones("8B", "") == 0.0
    assert camelot_to_semitones("?", "8B") == 0.0
    assert camelot_to_semitones("8B", "13A") == 0.0  # number out of range
    assert camelot_to_semitones("8C", "8B") == 0.0  # bad letter


def test_offset_is_always_in_wrap_window() -> None:
    # Stress: every pair of Camelot codes returns a value in [-6, 6].
    nums = list(range(1, 13))
    letters = ("A", "B")
    for a_n in nums:
        for a_l in letters:
            for b_n in nums:
                for b_l in letters:
                    out = camelot_to_semitones(f"{a_n}{a_l}", f"{b_n}{b_l}")
                    assert -6.0 <= out <= 6.0, (a_n, a_l, b_n, b_l, out)


# ---- key_match.compute_offset RPC tests -----------------------------


@pytest.mark.asyncio
async def test_rpc_returns_semitones_for_two_seeded_tracks(
    library: TrackLibrary,
) -> None:
    library.add_track(
        TrackRef("alpha", "/m/alpha.mp3", 120.0, "8B", 0.2, 200.0)
    )
    library.add_track(
        TrackRef("bravo", "/m/bravo.mp3", 124.0, "9B", 0.3, 210.0)
    )
    handler = LibraryRpcHandler(library)
    result = await handler.dispatch(
        "key_match.compute_offset",
        {"from_track_id": "alpha", "to_track_id": "bravo"},
    )
    assert result == {"semitones": -5.0}


@pytest.mark.asyncio
async def test_rpc_unknown_track_raises_invalid_params(
    library: TrackLibrary,
) -> None:
    library.add_track(
        TrackRef("alpha", "/m/alpha.mp3", 120.0, "8B", 0.2, 200.0)
    )
    handler = LibraryRpcHandler(library)
    with pytest.raises(RpcError) as exc_info:
        await handler.dispatch(
            "key_match.compute_offset",
            {"from_track_id": "alpha", "to_track_id": "does-not-exist"},
        )
    assert exc_info.value.code == JSONRPC_INVALID_PARAMS


def test_handler_registers_key_match_method() -> None:
    # The wire layer pre-filters via fully_qualified_methods — make
    # sure the new method shows up there so the dispatcher actually
    # routes it.
    lib = TrackLibrary(":memory:")
    handler = LibraryRpcHandler(lib)
    assert "key_match.compute_offset" in handler.fully_qualified_methods
    assert handler.handles("key_match.compute_offset")
