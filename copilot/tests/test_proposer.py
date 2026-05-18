"""TransitionProposer unit tests — pure, no network.

We seed an in-memory library, build a minimal :class:`EngineState`, call
the proposer once → assert the picked track + plan shape, then call it
again immediately → assert hysteresis suppresses the duplicate.
"""
from __future__ import annotations

import pytest

from copilot.library import TrackLibrary, TrackRef
from copilot.proposer import Proposal, TransitionProposer
from copilot.schemas import (
    Deck,
    DeckId,
    EngineState,
    TrackRef as EngineTrackRef,
)


@pytest.fixture
def proposer_library() -> TrackLibrary:
    lib = TrackLibrary(":memory:")
    # Playing track: 124 BPM, 8B, energy 0.20.
    lib.add_track(TrackRef("playing", "/playing.mp3", 124.0, "8B", 0.20, 210.0))
    # Best mashup pick: same key + 1 BPM up.
    lib.add_track(TrackRef("best", "/best.mp3", 125.0, "8B", 0.22, 220.0))
    # Runner-up: adjacent key, BPM slightly down.
    lib.add_track(TrackRef("runner_up", "/runner.mp3", 123.0, "9B", 0.18, 215.0))
    # Gated by BPM (24% off — outside ±8% gate).
    lib.add_track(TrackRef("too_fast", "/fast.mp3", 154.0, "8B", 0.20, 200.0))
    # Gated by key (distance 6).
    lib.add_track(TrackRef("bad_key", "/badkey.mp3", 124.5, "2A", 0.20, 200.0))
    try:
        yield lib
    finally:
        lib.close()


def _state_with_a_playing() -> EngineState:
    return EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="playing", path="/playing.mp3"),
            playing=True,
            position_ms=100_000,
            copilot_engaged=True,
            bpm=124.0,
        ),
        deck_b=Deck(copilot_engaged=True),
        session_active=True,
    )


def test_proposer_returns_top_ranked_candidate(proposer_library: TrackLibrary) -> None:
    proposer = TransitionProposer(proposer_library)
    state = _state_with_a_playing()
    out = proposer.on_state(state)

    assert isinstance(out, Proposal)
    assert out.next_track_id == "best", (
        f"expected best mashup pick to win, got {out.next_track_id}"
    )
    # Target deck is the OTHER deck (B, since A is playing).
    assert out.transition_plan.target_deck == DeckId.B
    # Confidence in [0, 1].
    assert 0.0 <= out.confidence <= 1.0
    # Best candidate's confidence should be high (small mashability penalty).
    assert out.confidence > 0.85, f"expected high confidence, got {out.confidence}"
    # Crossfader plan: from "A audible" (0.0) → "B audible" (1.0).
    # This matches decisions.transition_plan: target=B → from=0.0 → to=1.0.
    assert out.transition_plan.crossfader_from == 0.0
    assert out.transition_plan.crossfader_to == 1.0
    # 16 bars at ~484 ms/beat (124 BPM) → ~31s ramp.
    assert 25_000 <= out.transition_plan.crossfader_ramp_duration_ms <= 35_000
    # EQ swap at midpoint.
    assert (
        out.transition_plan.eq_swap_at_ms
        == out.transition_plan.crossfader_ramp_duration_ms // 2
    )
    # Pre-translated events exist.
    assert out.events, "proposer must attach the ready-to-send events"
    # First event is always the DeckLoad on the target deck.
    first_kind = out.events[0].kind.model_dump()
    assert first_kind["kind"] == "DeckLoad"
    assert first_kind["track"]["id"] == "best"


def test_proposer_hysteresis_within_window(proposer_library: TrackLibrary) -> None:
    """Two on_state() calls inside one beat must yield only one proposal."""
    # Use a manual clock so the test is deterministic. The proposer treats
    # the clock as monotonic seconds.
    fake_time = [0.0]

    def clock() -> float:
        return fake_time[0]

    proposer = TransitionProposer(proposer_library, _clock=clock)
    state = _state_with_a_playing()

    first = proposer.on_state(state)
    assert first is not None, "first call must produce a proposal"

    # Advance by 0.1s — well under the hysteresis window (8 beats at
    # 124 BPM ≈ 3.87s).
    fake_time[0] = 0.1
    second = proposer.on_state(state)
    assert second is None, "hysteresis should suppress immediate re-propose"

    # Advance past the window — proposer fires again.
    fake_time[0] = 5.0
    third = proposer.on_state(state)
    assert third is not None, "after cooldown, proposer should re-fire"
    assert third.next_track_id == first.next_track_id


def test_proposer_returns_none_when_no_deck_playing(
    proposer_library: TrackLibrary,
) -> None:
    proposer = TransitionProposer(proposer_library)
    state = EngineState(session_active=True)  # both decks empty
    assert proposer.on_state(state) is None


def test_proposer_returns_none_when_library_empty() -> None:
    lib = TrackLibrary(":memory:")
    try:
        proposer = TransitionProposer(lib)
        state = _state_with_a_playing()
        assert proposer.on_state(state) is None
    finally:
        lib.close()


def test_proposer_reset_clears_hysteresis(proposer_library: TrackLibrary) -> None:
    fake_time = [0.0]

    def clock() -> float:
        return fake_time[0]

    proposer = TransitionProposer(proposer_library, _clock=clock)
    state = _state_with_a_playing()
    first = proposer.on_state(state)
    assert first is not None

    # Same instant — would normally be suppressed.
    fake_time[0] = 0.01
    suppressed = proposer.on_state(state)
    assert suppressed is None

    # After reset (simulates a reconnect), proposer fires again at t=0.01.
    proposer.reset()
    after_reset = proposer.on_state(state)
    assert after_reset is not None
    assert after_reset.next_track_id == first.next_track_id


def test_proposer_different_track_bypasses_hysteresis(
    proposer_library: TrackLibrary,
) -> None:
    """If the ranker's top pick changes (e.g. someone added a better
    candidate), the proposer should surface it even inside the cooldown."""
    fake_time = [0.0]

    def clock() -> float:
        return fake_time[0]

    proposer = TransitionProposer(proposer_library, _clock=clock)
    state = _state_with_a_playing()
    first = proposer.on_state(state)
    assert first is not None
    assert first.next_track_id == "best"

    # Remove "best" from the library — next pick should be different.
    proposer_library._conn.execute("DELETE FROM tracks WHERE track_id = 'best'")
    proposer_library._conn.commit()

    fake_time[0] = 0.1  # still within cooldown
    second = proposer.on_state(state)
    assert second is not None, (
        "proposer should re-fire when the picked track changes"
    )
    assert second.next_track_id != "best"
