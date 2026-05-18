"""Transition proposer — wraps :func:`copilot.decisions.next_track_decision`
with hysteresis + a typed ``Proposal`` shape suitable for handing straight
to the engine.

The proposer is intentionally **synchronous and pure** apart from the
beat-clock check: the service layer calls ``on_state(state)`` from inside
the engine_client's async notification handler and gets back either
``None`` (nothing to do) or a :class:`Proposal` it can translate into
``engine.submit_event`` calls.

Hysteresis policy
-----------------

The engine broadcasts ``engine.state_changed`` on every accepted event,
which during a phrase-aligned transition can mean dozens of state-changed
frames per second. Re-running the mashability ranker that often is wasted
work and could spam the engine with redundant proposals if the decision
flips between two near-ties.

We therefore gate proposals behind a **beat-based cool-down**:

* Compute the playing deck's beat period from ``deck.bpm``
  (60_000 / bpm ms per beat; default 500ms = 120 BPM if unknown).
* Re-propose at most once every ``_HYSTERESIS_BEATS`` beats
  (default 8, ~4s at 120 BPM).

Beat-clock advances in wall-clock seconds (``time.monotonic()``), not in
``state.position_ms``, because the position resets on every track load
and would otherwise yield false "we just proposed" outcomes after a swap.
"""
from __future__ import annotations

import logging
import time
from dataclasses import dataclass
from typing import Optional

from .decisions import (
    MashabilityFactors,
    NextTrackPlan,
    next_track_decision,
    transition_plan,
)
from .library import TrackLibrary
from .schemas import (
    DeckId,
    EngineState,
    Event,
)

log = logging.getLogger(__name__)


# Cool-down measured in beats — see module docstring.
_HYSTERESIS_BEATS = 8

# Fallback beat period when the playing deck reports bpm <= 0. 120 BPM
# is the v1 default and the safest middle-of-the-road dance value.
_DEFAULT_BEAT_PERIOD_MS = 500.0


@dataclass(frozen=True)
class TransitionPlanShape:
    """Structured view of the transition that the proposer wants the
    engine to execute.

    The actual wire events are emitted by
    :func:`copilot.decisions.transition_plan`; this shape lives in the
    proposer's contract so callers / the UI can render a "what's coming"
    preview without re-deriving the math.
    """

    target_deck: DeckId
    crossfader_from: float
    crossfader_to: float
    crossfader_ramp_duration_ms: int
    eq_swap_at_ms: int
    beat_align_at_ms: int


@dataclass(frozen=True)
class Proposal:
    """The proposer's output — a typed bundle handed to the service layer.

    * ``next_track_id`` — library track id for the incoming track.
    * ``transition_plan`` — high-level transition description (see
      :class:`TransitionPlanShape`).
    * ``confidence`` — 0..1, derived from the mashability score
      (1.0 = perfect match; falls toward 0 as the penalty grows).
    * ``events`` — pre-translated list of engine events ready for
      ``engine.submit_event``. Kept attached so the service doesn't need
      to re-import :func:`transition_plan`.
    """

    next_track_id: str
    transition_plan: TransitionPlanShape
    confidence: float
    events: tuple[Event, ...]
    score: MashabilityFactors


@dataclass
class _PerDeckState:
    """Mutable per-deck bookkeeping (hysteresis clock + last pick)."""

    last_proposal_at_monotonic: float = 0.0
    last_track_id: Optional[str] = None
    # Beat period used at last proposal — kept so a tempo swap doesn't
    # silently extend / shrink the hysteresis window.
    last_beat_period_ms: float = _DEFAULT_BEAT_PERIOD_MS


class TransitionProposer:
    """Stateful wrapper around the pure decision functions.

    Held by the service for the lifetime of the connection; one instance
    per ``CoPilotService`` (NOT one per deck — both decks share the
    hysteresis bookkeeping to avoid double-firing in cross-deck cases).
    """

    def __init__(
        self,
        library: TrackLibrary,
        *,
        hysteresis_beats: int = _HYSTERESIS_BEATS,
        transition_bars: int = 16,
        _clock: Optional[object] = None,
    ) -> None:
        self._library = library
        self._hysteresis_beats = hysteresis_beats
        self._transition_bars = transition_bars
        # Indirection so tests can plug a manual clock; default is
        # ``time.monotonic``. Typed as ``object`` to keep the surface
        # narrow; the protocol is "callable returning float seconds".
        self._clock = _clock or time.monotonic
        # Per-deck bookkeeping is keyed by the *target* deck — the deck
        # the incoming track will land on. That matches how the hysteresis
        # decision is made: "did I just propose to load deck B?".
        self._per_deck: dict[DeckId, _PerDeckState] = {
            DeckId.A: _PerDeckState(),
            DeckId.B: _PerDeckState(),
        }

    # ------------------------------------------------------------------
    # Public surface
    # ------------------------------------------------------------------

    def on_state(self, state: EngineState) -> Optional[Proposal]:
        """Decide whether the current state warrants a fresh proposal.

        Returns ``None`` if:
          * No deck is playing.
          * Neither deck has co-pilot engaged.
          * The hysteresis window for the target deck has not elapsed.
          * The mashability ranker has no compatible track.

        Otherwise returns a :class:`Proposal` ready for the engine.
        """
        plan = next_track_decision(state, self._library)
        if plan is None:
            return None

        target_deck = plan.target_deck
        per_deck = self._per_deck[target_deck]

        # Beat period derived from the *playing* deck's reported BPM.
        # We pull it from whichever deck is currently active (the one the
        # decision was computed against).
        playing_deck = state.deck_a if state.deck_a.playing else state.deck_b
        beat_period_ms = (
            60_000.0 / playing_deck.bpm if playing_deck.bpm > 0 else _DEFAULT_BEAT_PERIOD_MS
        )

        # Hysteresis: enforce a minimum gap between proposals on the same
        # target deck. Same target + same picked track + within the cooldown
        # → skip. Different track is allowed through (the ranker decided
        # the previous pick is no longer optimal — e.g. someone added a
        # track to the library — and we want to surface that).
        now = float(self._clock())  # type: ignore[operator]
        if (
            per_deck.last_track_id == plan.incoming_track.track_id
            and now - per_deck.last_proposal_at_monotonic
            < self._cooldown_seconds(beat_period_ms)
        ):
            log.debug(
                "proposer: hysteresis suppressed re-proposal of %s on deck %s",
                plan.incoming_track.track_id,
                target_deck.value,
            )
            return None

        # Confidence: monotone-decreasing in total mashability penalty.
        # The penalty is bounded by gates (≤8% BPM × 10 + ≤2 key × 2 +
        # ≤~0.8 energy × 1.5 ≈ ≤ 3.4 in practice). Map [0, 3.4] → [1, 0].
        score_total = plan.score.total
        confidence = max(0.0, min(1.0, 1.0 - score_total / 3.5))

        events = tuple(
            transition_plan(state, plan, transition_bars=self._transition_bars)
        )

        shape = self._build_shape(plan, beat_period_ms)

        # Record bookkeeping for next call.
        per_deck.last_proposal_at_monotonic = now
        per_deck.last_track_id = plan.incoming_track.track_id
        per_deck.last_beat_period_ms = beat_period_ms

        return Proposal(
            next_track_id=plan.incoming_track.track_id,
            transition_plan=shape,
            confidence=confidence,
            events=events,
            score=plan.score,
        )

    def reset(self) -> None:
        """Forget hysteresis state — useful on reconnect, since the
        engine's state may have moved on while we were offline."""
        for deck in (DeckId.A, DeckId.B):
            self._per_deck[deck] = _PerDeckState()

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _cooldown_seconds(self, beat_period_ms: float) -> float:
        return (self._hysteresis_beats * beat_period_ms) / 1000.0

    def _build_shape(
        self, plan: NextTrackPlan, beat_period_ms: float
    ) -> TransitionPlanShape:
        target = plan.target_deck
        from_value = 0.0 if target == DeckId.B else 1.0
        to_value = 1.0 - from_value
        # Crossfader ramp covers `_transition_bars` bars at 4 beats/bar.
        ramp_ms = int(self._transition_bars * 4 * beat_period_ms)
        # EQ swap (bass kill on outgoing) lands at the midpoint of the
        # ramp — that's the v0.1 fixed policy; a stem-aware plan in v0.2
        # will compute this from the outgoing track's last-chorus end.
        eq_swap_at_ms = ramp_ms // 2
        # Beat-align: align the incoming start to the next downbeat. The
        # engine resolves "next downbeat" against its own clock; we tag
        # the event with intent rather than an absolute timestamp.
        beat_align_at_ms = 0
        return TransitionPlanShape(
            target_deck=target,
            crossfader_from=from_value,
            crossfader_to=to_value,
            crossfader_ramp_duration_ms=ramp_ms,
            eq_swap_at_ms=eq_swap_at_ms,
            beat_align_at_ms=beat_align_at_ms,
        )


__all__ = [
    "Proposal",
    "TransitionPlanShape",
    "TransitionProposer",
]
