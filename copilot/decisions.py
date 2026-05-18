"""Pure decision functions for the co-pilot.

These are kept side-effect-free so they're trivially unit-testable. The
service loop in :mod:`copilot.service` wires them to the WebSocket, but no
network or filesystem I/O happens inside the functions below.

v0.1 scope (per ADR-002 and the implementation task brief):

* ``next_track_decision`` picks the highest-mashability donor from the
  library that fits the currently-playing deck.
* ``transition_plan`` emits a stub 16-bar crossfade plan + phrase-aligned
  start cue. Real beat-matching (sub-bar phase alignment, tempo-aware
  envelope shaping, stem-aware fade curves) lands in a follow-up PR.

The mashability scoring function lives here, not in
``copilot.vendor.mashup``, because the v1 vendored ``mashup.py`` is the
*renderer*, not a scorer — v1 only ships compatibility math inside
``ordering.py`` (which isn't vendored, see VENDOR.md). We re-implement the
score here using the same factors (Camelot distance + BPM stretch + energy
delta) so the formula is auditable in one file.
"""
from __future__ import annotations

from dataclasses import dataclass

from .library import (
    TrackLibrary,
    TrackRef,
    bpm_stretch_ratio,
    camelot_distance,
)
from .schemas import (
    CopilotEngage,
    CrossfaderRamp,
    Deck,
    DeckId,
    DeckLoad,
    EngineState,
    Event,
    EventSource,
    LoopIn,
    LoopOut,
    TrackRef as EngineTrackRef,
)


# ---------- mashability scoring ----------


@dataclass(frozen=True)
class MashabilityFactors:
    """Breakdown of the score so the UI can show *why* a track was picked.

    Lower is better — score is a penalty, not a reward.

        * key_penalty   = Camelot distance × KEY_WEIGHT
        * bpm_penalty   = relative stretch × BPM_WEIGHT
        * energy_penalty = |energy delta| × ENERGY_WEIGHT (we want the next
          track to maintain or gently climb energy; large drops hurt the
          dancefloor more than small climbs).
    """

    key_penalty: float
    bpm_penalty: float
    energy_penalty: float

    @property
    def total(self) -> float:
        return self.key_penalty + self.bpm_penalty + self.energy_penalty


# Weights tuned against v1's `ordering.py` defaults (key x2, bpm x3,
# energy_arc x2.5). The v1 scorer also penalizes against an "energy arc
# direction" but that requires session history — co-pilot v0.1 keeps it
# simple and penalizes *magnitude* of the delta only. Will revisit when
# we wire in session memory.
_KEY_WEIGHT = 2.0
_BPM_WEIGHT = 10.0  # bpm_stretch_ratio is ~0..0.08, so weight is large to balance
_ENERGY_WEIGHT = 1.5


def mashability_score(
    playing_bpm: float,
    playing_camelot: str,
    playing_energy: float,
    candidate: TrackRef,
) -> MashabilityFactors:
    """Compute a mashability penalty for ``candidate`` against the playing
    track. Lower = better fit.

    Pure: no I/O, no global state, no randomness.
    """
    key_d = camelot_distance(playing_camelot, candidate.camelot_key)
    bpm_d = bpm_stretch_ratio(playing_bpm, candidate.bpm)
    energy_d = abs(candidate.energy - playing_energy)

    return MashabilityFactors(
        key_penalty=key_d * _KEY_WEIGHT,
        bpm_penalty=bpm_d * _BPM_WEIGHT,
        energy_penalty=energy_d * _ENERGY_WEIGHT,
    )


# ---------- next-track decision ----------


@dataclass(frozen=True)
class NextTrackPlan:
    """The picked-track output of :func:`next_track_decision`.

    Contains *just* the track choice + score breakdown. Translating the pick
    into engine events is the job of :func:`transition_plan` — keeping the
    two split makes both halves independently testable.
    """

    incoming_track: TrackRef
    target_deck: DeckId
    score: MashabilityFactors
    runner_ups: tuple[tuple[TrackRef, MashabilityFactors], ...]


def _active_playing_deck(state: EngineState) -> tuple[DeckId, Deck] | None:
    """Return the deck that's currently playing audio out front of house.

    Heuristic: a deck is "active" if ``playing == True`` and the crossfader
    is biased toward it (or sitting at 0.5 — center). When both decks are
    playing, pick the one with the higher crossfader weight.
    """
    da, db = state.deck_a, state.deck_b
    if da.playing and not db.playing:
        return DeckId.A, da
    if db.playing and not da.playing:
        return DeckId.B, db
    if da.playing and db.playing:
        # Pick whichever owns more of the master signal right now.
        return (DeckId.A, da) if state.crossfader < 0.5 else (DeckId.B, db)
    return None


def next_track_decision(
    state: EngineState,
    library: TrackLibrary,
    *,
    exclude_track_ids: set[str] | None = None,
    top_k_gate: int = 20,
) -> NextTrackPlan | None:
    """Pick the highest-mashability track for the upcoming transition.

    Returns ``None`` if no library track passes the BPM + key gates, or if
    no deck is currently playing.

    Pure function w.r.t. ``state`` and the library snapshot: calling it twice
    with the same inputs returns the same pick.
    """
    active = _active_playing_deck(state)
    if active is None:
        return None
    playing_deck_id, playing_deck = active
    loaded = playing_deck.loaded
    if loaded is None:
        return None

    # The incoming track goes on the *other* deck (B if A is playing,
    # or A if B is playing). ADR-002 makes this explicit.
    target_deck_id, _ = state.other_deck(playing_deck_id)

    # Exclude the currently playing track + anything the caller wants out.
    excluded = set(exclude_track_ids or ())
    excluded.add(loaded.id)
    # Also exclude whatever's loaded on the target deck so we don't reload
    # the same track on top of itself.
    other_loaded = state.deck(target_deck_id).loaded
    if other_loaded is not None:
        excluded.add(other_loaded.id)

    # Energy isn't in the engine state — the library row carries it. The
    # currently-playing deck's energy is approximated via the library row
    # for the loaded track (looked up here). If the lib doesn't know about
    # the playing track we fall back to median library energy.
    playing_lib = library.get(loaded.id)
    playing_energy = playing_lib.energy if playing_lib else _median_energy(library)

    # ADR review (Codex): the engine carries Camelot key on the loaded
    # TrackRef in a future PR. For v0.1 the engine TrackRef has only
    # {id, path}, so we look up the library entry for the key.
    if playing_lib is None:
        # Engine has loaded a track that's not in the co-pilot's library.
        # Without knowing its key we can't gate — bail loudly.
        return None
    playing_camelot = playing_lib.camelot_key
    playing_bpm = playing_deck.bpm or playing_lib.bpm

    candidates = library.pick_compatible_for(
        playing_bpm=playing_bpm,
        playing_camelot=playing_camelot,
        exclude_ids=excluded,
        top_k=top_k_gate,
    )
    if not candidates:
        return None

    scored = [
        (c, mashability_score(playing_bpm, playing_camelot, playing_energy, c))
        for c in candidates
    ]
    scored.sort(key=lambda t: t[1].total)
    best, best_score = scored[0]
    runner_ups = tuple(scored[1:6])  # keep up to 5 alternates for UI / logs

    return NextTrackPlan(
        incoming_track=best,
        target_deck=target_deck_id,
        score=best_score,
        runner_ups=runner_ups,
    )


def _median_energy(library: TrackLibrary) -> float:
    rows = library.all_tracks()
    if not rows:
        return 0.1
    energies = sorted(r.energy for r in rows)
    return energies[len(energies) // 2]


# ---------- transition plan ----------


# v0.1 transition shape — fixed 16-bar crossfade after a phrase-aligned start.
# Documented limitations:
#   * No tempo matching beyond what the engine's playback rate does — we don't
#     emit pitch_semitones changes.
#   * No phase nudge — assumes the engine's clock-sync ADR-007 has aligned
#     downbeats before we run.
#   * No stem-aware EQ swap (kill bass on outgoing during the second half of
#     the crossfade). EQ events ship in v0.2 once we have a stem-aware lib.
_DEFAULT_TRANSITION_BARS = 16


def transition_plan(
    state: EngineState,
    plan: NextTrackPlan,
    *,
    transition_bars: int = _DEFAULT_TRANSITION_BARS,
) -> list[Event]:
    """Translate a ``NextTrackPlan`` into a list of engine events.

    The returned list is ordered: the engine processes events in sequence,
    so each subsequent event applies to the state produced by the prior.

    v0.1 emits:
      1. ``DeckLoad`` on the target deck (incoming track, BPM, beat-grid
         anchor placeholder — engine's audio thread will refine on load).
      2. ``CopilotEngage`` on the target deck so the engine knows the AI
         owns it.
      3. ``LoopIn`` / ``LoopOut`` on the *current* deck near its outro
         (a defensive 4-bar safety loop the engine can engage if the
         co-pilot service stalls mid-transition — a Carmack-style "fail
         to a benign state").
      4. ``DeckPlay`` on the target deck.
      5. ``CrossfaderRamp`` describing the 16-bar fade. The engine
         expands this into discrete ``Crossfader`` events at the next
         phrase boundary (ADR-007).
    """
    events: list[Event] = []

    incoming = plan.incoming_track
    target_deck = plan.target_deck

    # Figure out the source-of-truth playing deck (we already validated in
    # next_track_decision that this exists).
    active = _active_playing_deck(state)
    assert active is not None, "transition_plan called without an active deck"
    playing_deck_id, _playing_deck = active

    # Crossfader direction: A->B means xfader 0.0 -> 1.0, B->A means 1.0 -> 0.0.
    from_value = 0.0 if target_deck == DeckId.B else 1.0
    to_value = 1.0 - from_value

    # 1. Load the incoming track on the target deck.
    events.append(
        Event(
            source=EventSource.Copilot,
            kind=DeckLoad(
                deck=target_deck,
                track=EngineTrackRef(id=incoming.track_id, path=incoming.path),
                bpm=incoming.bpm,
                # Beat-grid anchor: 0 is a "best effort" placeholder — the
                # engine's analyzer cache (HypeHouse v1 carry-over) is the
                # canonical source. The engine reducer accepts what we send;
                # if it has a cached value it'll override on load.
                beat_grid_anchor_ms=0,
            ),
        )
    )

    # 2. Engage co-pilot on the target deck.
    events.append(
        Event(source=EventSource.Copilot, kind=CopilotEngage(deck=target_deck))
    )

    # 3. Safety loop on outgoing — engine can fall back to it if the co-pilot
    #    process dies mid-transition. The engine treats LoopIn/Out as a pair;
    #    LoopOut at the same position arms the loop but doesn't enter it.
    events.append(
        Event(source=EventSource.Copilot, kind=LoopIn(deck=playing_deck_id))
    )
    events.append(
        Event(source=EventSource.Copilot, kind=LoopOut(deck=playing_deck_id))
    )

    # 4. Start the incoming track. Engine will phrase-align on receipt.
    from .schemas import DeckPlay  # local import to avoid widening top-level

    events.append(
        Event(source=EventSource.Copilot, kind=DeckPlay(deck=target_deck))
    )

    # 5. Crossfader ramp — engine expands into discrete Crossfader events at
    #    the next phrase boundary.
    events.append(
        Event(
            source=EventSource.Copilot,
            kind=CrossfaderRamp(
                from_value=from_value,
                to_value=to_value,
                duration_bars=transition_bars,
                start_at_phrase_boundary=True,
            ),
        )
    )
    return events
