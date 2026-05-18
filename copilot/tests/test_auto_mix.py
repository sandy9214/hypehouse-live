"""AutoMixController unit tests — pure, no network.

Each test wires the controller to:
  * a real :class:`TransitionProposer` (the seeded test library is small
    enough that the ranker is deterministic),
  * a list-collecting fake ``submit_event`` so we can assert the event
    sequence the controller would have pushed to the engine,
  * an optional state-changed collector to assert the wire shape of
    ``copilot.auto_mix_state_changed`` notifications.
"""
from __future__ import annotations

import asyncio

import pytest

from copilot.auto_mix import (
    AutoMixController,
    AutoMixStatus,
    DEFAULT_LOOKAHEAD_MS,
)
from copilot.library import TrackLibrary, TrackRef
from copilot.proposer import TransitionProposer
from copilot.schemas import (
    Deck,
    DeckId,
    EngineState,
    Event,
    TrackRef as EngineTrackRef,
)


# ---------- fixtures ----------


@pytest.fixture
def seeded_library() -> TrackLibrary:
    lib = TrackLibrary(":memory:")
    lib.add_track(
        TrackRef("playing", "/playing.mp3", 124.0, "8B", 0.20, 210.0)
    )
    lib.add_track(
        TrackRef("best", "/best.mp3", 125.0, "8B", 0.22, 220.0)
    )
    lib.add_track(
        TrackRef("playing_b", "/playing_b.mp3", 126.0, "9B", 0.30, 200.0)
    )
    lib.add_track(
        TrackRef("best_for_b", "/best_for_b.mp3", 127.0, "9B", 0.28, 210.0)
    )
    try:
        yield lib
    finally:
        lib.close()


def _make_controller(
    library: TrackLibrary,
) -> tuple[AutoMixController, list[Event], list[dict[str, object]]]:
    """Build a controller + sinks for emitted events + state-changed pings."""
    submitted: list[Event] = []
    pings: list[dict[str, object]] = []

    async def submit(ev: Event) -> None:
        submitted.append(ev)

    async def on_change(
        deck_id: DeckId,
        status: AutoMixStatus,
        seconds_to_mix: int | None,
    ) -> None:
        pings.append(
            {
                "deck": deck_id.value,
                "status": status.value,
                "seconds_to_mix": seconds_to_mix,
            }
        )

    proposer = TransitionProposer(library)
    ctrl = AutoMixController(proposer, submit, state_changed=on_change)
    return ctrl, submitted, pings


def _far_from_end_state() -> EngineState:
    """Deck A playing, 30s into a 210s track — well outside the look-ahead."""
    return EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="playing", path="/playing.mp3"),
            playing=True,
            position_ms=30_000,
            copilot_engaged=True,
            bpm=124.0,
        ),
        deck_b=Deck(copilot_engaged=True),
        session_active=True,
    )


def _near_end_state(position_ms: int = 190_000) -> EngineState:
    """Deck A playing, inside the look-ahead window."""
    return EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="playing", path="/playing.mp3"),
            playing=True,
            position_ms=position_ms,
            copilot_engaged=True,
            bpm=124.0,
        ),
        deck_b=Deck(copilot_engaged=True),
        session_active=True,
    )


# ---------- tests ----------


async def test_tick_far_from_end_is_idle_no_action(
    seeded_library: TrackLibrary,
) -> None:
    """Outside the look-ahead window the controller stays IDLE + emits
    no engine events and no state-changed pings."""
    ctrl, submitted, pings = _make_controller(seeded_library)
    ctrl.set_auto_mix(DeckId.A, True)
    # Toggle-on triggers a notify; drop those from this assertion.
    await asyncio.sleep(0)
    starting_pings = len(pings)

    await ctrl.tick(_far_from_end_state())
    # Yield to let any background tasks settle (none should have spawned).
    await asyncio.sleep(0)

    snapshot = ctrl.get_auto_mix(DeckId.A)
    assert snapshot["status"] == AutoMixStatus.IDLE.value
    assert snapshot["seconds_to_mix"] is None
    assert submitted == [], "no events should be submitted yet"
    # No further state-changed pings beyond the toggle-on broadcast.
    assert len(pings) == starting_pings


async def test_tick_near_end_arms_and_executes(
    seeded_library: TrackLibrary,
) -> None:
    """Inside the look-ahead window with auto-mix on + copilot engaged,
    the controller proposes + executes. The engine events submitted must
    match what the proposer's plan generated."""
    ctrl, submitted, pings = _make_controller(seeded_library)
    ctrl.set_auto_mix(DeckId.A, True)

    state = _near_end_state(position_ms=190_000)  # 20s remaining
    await ctrl.tick(state)
    # The execute() coroutine spawns as a task; let it run to completion.
    # Drain any pending tasks created via asyncio.create_task.
    for _ in range(5):
        await asyncio.sleep(0)

    snapshot = ctrl.get_auto_mix(DeckId.A)
    # Either TRANSITIONING (still in flight) or DONE; both prove armed.
    assert snapshot["status"] in (
        AutoMixStatus.TRANSITIONING.value,
        AutoMixStatus.DONE.value,
    )
    assert len(submitted) > 0, "execute() should have submitted events"
    # First event is always DeckLoad on the OTHER deck.
    first = submitted[0].kind.model_dump()
    assert first["kind"] == "DeckLoad"
    assert first["deck"] == "B"
    assert first["track"]["id"] == "best"

    # State-changed pings include both TRANSITIONING and DONE (or at
    # least ARMED + TRANSITIONING + DONE depending on scheduling). The
    # invariant is: the most-recent ping for deck A is DONE or
    # TRANSITIONING.
    a_pings = [p for p in pings if p["deck"] == "A"]
    statuses = [p["status"] for p in a_pings]
    assert AutoMixStatus.ARMED.value in statuses
    assert AutoMixStatus.TRANSITIONING.value in statuses


async def test_tick_no_action_without_copilot_engaged(
    seeded_library: TrackLibrary,
) -> None:
    """Even with auto-mix on, copilot_engaged=False suppresses the trigger.

    Mirrors the proposer's design: the operator's per-deck CO-PILOT
    toggle remains authoritative.
    """
    ctrl, submitted, _pings = _make_controller(seeded_library)
    ctrl.set_auto_mix(DeckId.A, True)

    state = EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="playing", path="/playing.mp3"),
            playing=True,
            position_ms=190_000,
            copilot_engaged=False,  # operator opted OUT
            bpm=124.0,
        ),
        deck_b=Deck(copilot_engaged=True),
        session_active=True,
    )
    await ctrl.tick(state)
    for _ in range(3):
        await asyncio.sleep(0)
    assert submitted == []
    assert ctrl.get_auto_mix(DeckId.A)["status"] == AutoMixStatus.IDLE.value


async def test_disable_mid_transition_cancels_remaining_events(
    seeded_library: TrackLibrary,
) -> None:
    """Flipping auto-mix off after ARMED but before the last event lands
    cancels the in-flight task; events already submitted stay applied."""
    # Build a controller with a SLOW submit_event so we can interleave
    # the set_auto_mix(off) call between events.
    submitted: list[Event] = []
    submit_started = asyncio.Event()
    submit_unblock = asyncio.Event()

    async def slow_submit(ev: Event) -> None:
        submitted.append(ev)
        # Pause after first event so the test can disable mid-flight.
        if len(submitted) == 1:
            submit_started.set()
            await submit_unblock.wait()

    ctrl = AutoMixController(
        TransitionProposer(seeded_library),
        slow_submit,
        state_changed=None,
    )
    ctrl.set_auto_mix(DeckId.A, True)
    await ctrl.tick(_near_end_state())
    # Wait for the execute task to land its first event.
    await asyncio.wait_for(submit_started.wait(), timeout=1.0)
    assert len(submitted) == 1

    # Disable while the slow submit is parked.
    ctrl.set_auto_mix(DeckId.A, False)
    # Unblocking the submit lets the task observe the cancellation.
    submit_unblock.set()
    # Yield enough to flush the cancellation.
    for _ in range(10):
        await asyncio.sleep(0)

    # Only the first event landed; later events were cancelled.
    assert len(submitted) == 1
    snapshot = ctrl.get_auto_mix(DeckId.A)
    assert snapshot["enabled"] is False
    assert snapshot["status"] == AutoMixStatus.IDLE.value


async def test_multi_deck_independence(
    seeded_library: TrackLibrary,
) -> None:
    """Auto-mix toggles on deck A must not affect deck B's state."""
    ctrl, _submitted, _pings = _make_controller(seeded_library)
    ctrl.set_auto_mix(DeckId.A, True)
    a = ctrl.get_auto_mix(DeckId.A)
    b = ctrl.get_auto_mix(DeckId.B)
    assert a["enabled"] is True
    assert b["enabled"] is False
    # Toggling B doesn't change A.
    ctrl.set_auto_mix(DeckId.B, True)
    assert ctrl.get_auto_mix(DeckId.A)["enabled"] is True
    assert ctrl.get_auto_mix(DeckId.B)["enabled"] is True
    # Disable B; A stays on.
    ctrl.set_auto_mix(DeckId.B, False)
    assert ctrl.get_auto_mix(DeckId.A)["enabled"] is True
    assert ctrl.get_auto_mix(DeckId.B)["enabled"] is False


async def test_proposer_returns_none_is_noop(
    seeded_library: TrackLibrary,
) -> None:
    """When the proposer returns None (e.g. library has no compatible
    track) the controller stays IDLE and emits no events."""
    # Empty library — proposer can't pick anything.
    lib = TrackLibrary(":memory:")
    # The playing track must exist in the library or proposer bails.
    lib.add_track(TrackRef("playing", "/playing.mp3", 124.0, "8B", 0.20, 210.0))
    # No compatible candidates added.
    try:
        submitted: list[Event] = []

        async def submit(ev: Event) -> None:
            submitted.append(ev)

        proposer = TransitionProposer(lib)
        ctrl = AutoMixController(proposer, submit)
        ctrl.set_auto_mix(DeckId.A, True)
        await ctrl.tick(_near_end_state())
        for _ in range(3):
            await asyncio.sleep(0)
        assert submitted == []
        assert ctrl.get_auto_mix(DeckId.A)["status"] == AutoMixStatus.IDLE.value
    finally:
        lib.close()


async def test_completion_resets_to_idle(
    seeded_library: TrackLibrary,
) -> None:
    """Once the playing deck reports a different loaded track (transition
    done), the state machine resets to IDLE so the NEXT track can re-arm."""
    ctrl, _submitted, _pings = _make_controller(seeded_library)
    ctrl.set_auto_mix(DeckId.A, True)
    await ctrl.tick(_near_end_state())
    for _ in range(5):
        await asyncio.sleep(0)

    # Now the engine reports deck A loaded with a DIFFERENT track —
    # simulating a manual reload between transitions.
    new_state = EngineState(
        deck_a=Deck(
            loaded=EngineTrackRef(id="something_else", path="/x.mp3"),
            playing=True,
            position_ms=5_000,
            copilot_engaged=True,
            bpm=124.0,
        ),
        deck_b=Deck(copilot_engaged=True),
        session_active=True,
    )
    await ctrl.tick(new_state)
    for _ in range(3):
        await asyncio.sleep(0)
    snapshot = ctrl.get_auto_mix(DeckId.A)
    assert snapshot["status"] == AutoMixStatus.IDLE.value


async def test_seconds_to_mix_countdown_decreases(
    seeded_library: TrackLibrary,
) -> None:
    """The state-changed pings carry seconds_to_mix; as position advances
    the value should monotonically decrease toward zero."""
    ctrl, _submitted, pings = _make_controller(seeded_library)
    ctrl.set_auto_mix(DeckId.A, True)

    # Three ticks at increasing positions, all inside the look-ahead.
    for pos in (185_000, 188_000, 195_000):
        await ctrl.tick(_near_end_state(position_ms=pos))
        await asyncio.sleep(0)

    seconds_values = [
        p["seconds_to_mix"] for p in pings
        if p["deck"] == "A" and isinstance(p["seconds_to_mix"], int)
    ]
    assert len(seconds_values) >= 2
    # Monotone non-increasing.
    for a, b in zip(seconds_values, seconds_values[1:], strict=False):
        assert b <= a, f"countdown should not increase: {seconds_values}"


async def test_get_auto_mix_initial_shape(
    seeded_library: TrackLibrary,
) -> None:
    """The wire shape of ``get_auto_mix`` is stable from boot."""
    ctrl, _submitted, _pings = _make_controller(seeded_library)
    snapshot = ctrl.get_auto_mix(DeckId.A)
    assert snapshot == {
        "deck": "A",
        "enabled": False,
        "status": "idle",
        "seconds_to_mix": None,
    }


async def test_default_lookahead_value() -> None:
    """30s is documented as the look-ahead default — guard against drift."""
    assert DEFAULT_LOOKAHEAD_MS == 30_000
