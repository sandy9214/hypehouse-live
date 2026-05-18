"""Decision-function tests.

These are the heart of the co-pilot's behaviour. Anything the network loop
calls must stay testable here with zero I/O — if a test ever needs to monkey-
patch decisions.py, the design is wrong.
"""
from __future__ import annotations

from copilot.decisions import (
    mashability_score,
    next_track_decision,
    transition_plan,
)
from copilot.library import TrackLibrary, TrackRef
from copilot.schemas import (
    CrossfaderRamp,
    Deck,
    DeckId,
    DeckLoad,
    EngineState,
    TrackRef as EngineTrackRef,
)


def _state_with_a_playing(playing_track_id: str = "t1") -> EngineState:
    """Build an EngineState where deck A is playing ``playing_track_id`` and
    co-pilot is engaged. Position is past the trigger threshold so the
    transition logic kicks in.
    """
    return EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id=playing_track_id, path=f"/tracks/{playing_track_id}.mp3"),
            playing=True,
            position_ms=190_000,  # 190s into a 210s track — within 30s of end
            copilot_engaged=True,
            bpm=124.0,
            beat_period_ms=60_000.0 / 124.0,
        ),
        deck_b=Deck(),
        crossfader=0.0,
        session_active=True,
    )


# -------- mashability_score: pure unit tests --------


def test_mashability_score_lower_is_better():
    """Identical track scores 0; mismatched scores higher."""
    candidate_perfect = TrackRef("p", "/p.mp3", 124.0, "8B", 0.20, 200.0)
    candidate_off = TrackRef("o", "/o.mp3", 130.0, "10B", 0.40, 200.0)

    perfect = mashability_score(124.0, "8B", 0.20, candidate_perfect)
    off = mashability_score(124.0, "8B", 0.20, candidate_off)

    assert perfect.total == 0.0
    assert off.total > perfect.total


def test_mashability_score_components_breakdown():
    """All three penalty buckets contribute independently."""
    # Key-only mismatch (1 step on Camelot wheel).
    key_only = mashability_score(
        124.0, "8B", 0.20, TrackRef("x", "/x.mp3", 124.0, "9B", 0.20, 200.0)
    )
    # BPM-only mismatch (~4%).
    bpm_only = mashability_score(
        124.0, "8B", 0.20, TrackRef("x", "/x.mp3", 129.0, "8B", 0.20, 200.0)
    )
    # Energy-only mismatch.
    energy_only = mashability_score(
        124.0, "8B", 0.20, TrackRef("x", "/x.mp3", 124.0, "8B", 0.50, 200.0)
    )

    assert key_only.key_penalty > 0 and key_only.bpm_penalty == 0 and key_only.energy_penalty == 0
    assert bpm_only.bpm_penalty > 0 and bpm_only.key_penalty == 0 and bpm_only.energy_penalty == 0
    assert energy_only.energy_penalty > 0 and energy_only.key_penalty == 0 and energy_only.bpm_penalty == 0


# -------- next_track_decision: 3-row synthetic library --------


def test_next_track_decision_picks_highest_mashability(library: TrackLibrary):
    """Synth 3 tracks with deliberately ordered mashability — assert the
    decision picks the best (lowest-penalty) one.

    Playing: t1 @ 124 BPM, 8B, energy 0.20.

    Library candidates (excluding t1 itself):
      * t2: 125 BPM / 8B / energy 0.21 — basically a clone. Highest mashability.
      * t3: 124 BPM / 9B / energy 0.20 — adjacent key only.
      * t4: 128 BPM / 8B / energy 0.20 — moderate BPM stretch.
    """
    library.add_track(TrackRef("t1", "/t1.mp3", 124.0, "8B", 0.20, 210.0))
    library.add_track(TrackRef("t2", "/t2.mp3", 125.0, "8B", 0.21, 220.0))
    library.add_track(TrackRef("t3", "/t3.mp3", 124.0, "9B", 0.20, 220.0))
    library.add_track(TrackRef("t4", "/t4.mp3", 128.0, "8B", 0.20, 220.0))

    state = _state_with_a_playing("t1")
    plan = next_track_decision(state, library)

    assert plan is not None
    assert plan.incoming_track.track_id == "t2"
    assert plan.target_deck == DeckId.B
    # Best score must beat all runner-ups.
    assert all(plan.score.total <= r[1].total for r in plan.runner_ups)


def test_next_track_decision_returns_none_when_no_deck_playing(library: TrackLibrary):
    library.add_track(TrackRef("t1", "/t1.mp3", 124.0, "8B", 0.20, 210.0))
    state = EngineState()  # nothing playing
    assert next_track_decision(state, library) is None


def test_next_track_decision_excludes_currently_loaded_tracks(library: TrackLibrary):
    """Track loaded on deck A *and* deck B must both be excluded so the
    co-pilot doesn't pick its own playing track or pre-stage a duplicate."""
    library.add_track(TrackRef("t1", "/t1.mp3", 124.0, "8B", 0.20, 210.0))
    library.add_track(TrackRef("t2", "/t2.mp3", 125.0, "8B", 0.21, 220.0))
    library.add_track(TrackRef("t3", "/t3.mp3", 124.0, "9B", 0.20, 220.0))

    state = EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="t1", path="/t1.mp3"),
            playing=True, position_ms=190_000, copilot_engaged=True, bpm=124.0,
        ),
        deck_b=Deck(
            loaded=EngineTrackRef(id="t2", path="/t2.mp3"),
            playing=False, position_ms=0, copilot_engaged=False, bpm=125.0,
        ),
        crossfader=0.0,
        session_active=True,
    )
    plan = next_track_decision(state, library)
    assert plan is not None
    assert plan.incoming_track.track_id == "t3"  # t1/t2 excluded


def test_next_track_decision_returns_none_when_no_compatible_candidates(library: TrackLibrary):
    """t1 plays, the only other track is far away in BPM — co-pilot bails."""
    library.add_track(TrackRef("t1", "/t1.mp3", 124.0, "8B", 0.20, 210.0))
    library.add_track(TrackRef("t_far", "/far.mp3", 200.0, "8B", 0.20, 220.0))

    state = _state_with_a_playing("t1")
    plan = next_track_decision(state, library)
    assert plan is None


# -------- transition_plan: shape of emitted events --------


def test_transition_plan_emits_expected_event_sequence(library: TrackLibrary):
    library.add_track(TrackRef("t1", "/t1.mp3", 124.0, "8B", 0.20, 210.0))
    library.add_track(TrackRef("t2", "/t2.mp3", 125.0, "8B", 0.21, 220.0))
    state = _state_with_a_playing("t1")

    plan = next_track_decision(state, library)
    assert plan is not None
    events = transition_plan(state, plan)

    # Expected sequence: DeckLoad, CopilotEngage, LoopIn, LoopOut, DeckPlay, CrossfaderRamp.
    kinds = [type(e.kind).__name__ for e in events]
    assert kinds == [
        "DeckLoad",
        "CopilotEngage",
        "LoopIn",
        "LoopOut",
        "DeckPlay",
        "CrossfaderRamp",
    ]
    # DeckLoad targets the inactive deck (B since A is playing).
    deck_load = events[0].kind
    assert isinstance(deck_load, DeckLoad)
    assert deck_load.deck == DeckId.B
    assert deck_load.track.id == "t2"
    # CrossfaderRamp goes from 0.0 (full A) to 1.0 (full B).
    ramp = events[-1].kind
    assert isinstance(ramp, CrossfaderRamp)
    assert ramp.from_value == 0.0
    assert ramp.to_value == 1.0
    assert ramp.duration_bars == 16
    assert ramp.start_at_phrase_boundary is True


def test_transition_plan_reverses_crossfader_when_deck_b_is_playing(library: TrackLibrary):
    """If deck B is the active deck, ramp must run 1.0 → 0.0 (back to A)."""
    library.add_track(TrackRef("t1", "/t1.mp3", 124.0, "8B", 0.20, 210.0))
    library.add_track(TrackRef("t2", "/t2.mp3", 125.0, "8B", 0.21, 220.0))
    state = EngineState(
        deck_a=Deck(),
        deck_b=Deck(
            loaded=EngineTrackRef(id="t1", path="/t1.mp3"),
            playing=True, position_ms=190_000, copilot_engaged=True, bpm=124.0,
        ),
        crossfader=1.0,
        session_active=True,
    )
    plan = next_track_decision(state, library)
    assert plan is not None
    events = transition_plan(state, plan)
    ramp = events[-1].kind
    assert isinstance(ramp, CrossfaderRamp)
    assert ramp.from_value == 1.0
    assert ramp.to_value == 0.0
    # Target deck is A.
    deck_load = events[0].kind
    assert isinstance(deck_load, DeckLoad)
    assert deck_load.deck == DeckId.A


def test_transition_plan_event_source_is_copilot(library: TrackLibrary):
    """All emitted events must be tagged source=Copilot so the engine can
    distinguish them from manual UI/MIDI events in the event log."""
    library.add_track(TrackRef("t1", "/t1.mp3", 124.0, "8B", 0.20, 210.0))
    library.add_track(TrackRef("t2", "/t2.mp3", 125.0, "8B", 0.21, 220.0))
    state = _state_with_a_playing("t1")

    plan = next_track_decision(state, library)
    events = transition_plan(state, plan)
    assert all(e.source.value == "Copilot" for e in events)
