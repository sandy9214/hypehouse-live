"""Auto-Mix controller — executes proposer plans without user prompts.

The :class:`TransitionProposer` (see ``proposer.py``) returns a typed
``Proposal`` whenever the currently-playing deck nears its end. PR #25
landed the proposer; PR #46 added persistence. This module wires the
final mile: when the operator opts a deck into **auto-mix**, the
``AutoMixController`` translates the proposer's plan into engine events
(``DeckLoad`` → ``DeckPlay`` → ``CrossfaderRamp``) and submits them
without waiting for confirmation.

State machine (per-deck, keyed by the playing deck):

    IDLE ──tick(near end + auto-mix on + copilot engaged)──▶ PROPOSED
    PROPOSED ──submit DeckLoad / DeckPlay / Crossfader──▶ EXECUTING
    EXECUTING ──all events accepted─▶ DONE
    DONE ──playing deck swapped (new track loaded)─▶ IDLE

Only the playing deck (the one losing focus) advances through the
states; the controller bookkeeping is keyed by the *source* deck id
because that's the deck where the trigger condition (near-end position)
is observed. The target deck is supplied by the proposer.

Why a separate module from ``service.py``?
  * Keeps the proposer wiring pure (``service.py`` already owns the
    ``run_with_proposer`` loop; this is the optional layer that runs on
    top).
  * Makes the state machine independently unit-testable without spinning
    up an engine WS — tests pass in a fake ``send_event`` callable plus
    a synthesized :class:`EngineState`.
  * Carmack-style "fail to a benign state": if the controller raises
    mid-transition we fall back to IDLE and the operator's existing
    proposer suggestion path is unaffected.
"""
from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from enum import Enum
from typing import Awaitable, Callable, Optional

from .proposer import Proposal, TransitionProposer
from .schemas import DeckId, EngineState, Event

log = logging.getLogger(__name__)


# Default look-ahead: when ``position_ms > duration_ms - 30_000`` the
# playing deck is considered "near end" and a proposal is sought. Matches
# the legacy service trigger so user expectations stay consistent.
DEFAULT_LOOKAHEAD_MS = 30_000


class AutoMixStatus(str, Enum):
    """Per-source-deck state-machine label.

    String-valued so the wire shape (notification payloads) carries a
    stable, debuggable token rather than an opaque integer.
    """

    IDLE = "idle"
    ARMED = "armed"  # = PROPOSED in the design doc, "armed" reads better in UI
    TRANSITIONING = "transitioning"  # = EXECUTING
    DONE = "done"


@dataclass
class _DeckAutoMixState:
    """Per-deck bookkeeping for the controller.

    ``enabled`` is the opt-in flag toggled via ``copilot.set_auto_mix``.
    ``status`` is the state-machine label.
    ``last_track_id`` is the loaded-track id captured the last time we
    advanced to TRANSITIONING; once the deck reports a different loaded
    track we know the transition completed and we reset to IDLE so the
    next near-end window re-arms cleanly.
    ``in_flight_task`` keeps a reference to the asyncio task currently
    submitting events — so that toggling auto-mix off mid-transition can
    cancel the remainder.
    """

    enabled: bool = False
    status: AutoMixStatus = AutoMixStatus.IDLE
    last_track_id: Optional[str] = None
    last_seconds_to_mix: Optional[int] = None
    in_flight_task: Optional[asyncio.Task[None]] = field(default=None, repr=False)


# Callable signature for "submit one engine event". Mirrors
# ``EngineClient.call("engine.submit_event", {"event": ...})``; kept
# narrow here so tests can inject a list-collecting stub without
# constructing a full :class:`EngineClient`.
SubmitEventFn = Callable[[Event], Awaitable[None]]
# Callable signature for "notify the UI that auto-mix state advanced".
# Receives ``(deck_id, status, seconds_to_mix)``. Implementation pushes
# the params over the JSON-RPC notification channel — see
# ``service.py``'s wiring.
StateChangedFn = Callable[[DeckId, AutoMixStatus, Optional[int]], Awaitable[None]]


class AutoMixController:
    """Drives the proposer's plan onto the engine when auto-mix is on.

    Lifecycle:

        ctrl = AutoMixController(proposer, submit_event)
        ctrl.set_auto_mix(DeckId.A, True)
        await ctrl.tick(state)   # called from the run_with_proposer loop

    The controller is **owned** by :class:`CoPilotService`; one instance
    per service. Both decks share the controller — per-deck state lives
    in ``self._decks``.

    Idempotency: ``tick`` is safe to call repeatedly. Once a transition
    has been launched for a given source deck, subsequent ticks short-
    circuit until the deck reports a different loaded track (proxy for
    "the transition completed"). This mirrors the legacy
    ``_decision_in_flight`` guard in ``service.py``.
    """

    def __init__(
        self,
        proposer: TransitionProposer,
        submit_event: SubmitEventFn,
        *,
        state_changed: Optional[StateChangedFn] = None,
        lookahead_ms: int = DEFAULT_LOOKAHEAD_MS,
        _clock: Optional[Callable[[], float]] = None,
    ) -> None:
        self._proposer = proposer
        self._submit_event = submit_event
        self._state_changed = state_changed
        self._lookahead_ms = lookahead_ms
        self._clock = _clock or time.monotonic
        # Per-source-deck state — auto-mix is opt-in PER deck, so both
        # decks need their own bookkeeping. Indexed by the *playing*
        # deck (the one whose end-of-track triggers the mix).
        self._decks: dict[DeckId, _DeckAutoMixState] = {
            DeckId.A: _DeckAutoMixState(),
            DeckId.B: _DeckAutoMixState(),
        }

    # ------------------------------------------------------------------
    # Public surface — RPC handlers (set_auto_mix / get_auto_mix) call into
    # these. Synchronous because the underlying mutation is trivially
    # in-memory; the notification fan-out (if any) is fire-and-forget.
    # ------------------------------------------------------------------

    def set_auto_mix(self, deck_id: DeckId, enabled: bool) -> None:
        """Toggle auto-mix for ``deck_id``.

        Turning OFF mid-transition cancels the in-flight task so any
        remaining ``CrossfaderRamp`` / ``DeckPlay`` events are NOT
        submitted. The events already submitted (typically ``DeckLoad``)
        stay applied — there's no engine-side "undo" today.

        Toggling ON is benign: the next ``tick`` will pick up the new
        flag without restarting any state machine.
        """
        per = self._decks[deck_id]
        was_enabled = per.enabled
        per.enabled = enabled
        if not enabled and per.in_flight_task is not None:
            task = per.in_flight_task
            if not task.done():
                task.cancel()
            per.in_flight_task = None
            # Force state back to IDLE so the operator can re-arm cleanly
            # after disabling.
            per.status = AutoMixStatus.IDLE
            per.last_seconds_to_mix = None
        if was_enabled != enabled:
            # Surface the change to subscribers — even when the operator
            # toggles on/off without crossing a state-machine boundary
            # the UI wants to update the pulse animation.
            self._schedule_notify(deck_id, per.status, per.last_seconds_to_mix)

    def get_auto_mix(self, deck_id: DeckId) -> dict[str, object]:
        """Wire shape for ``copilot.get_auto_mix`` — read-only view.

        Returns the bool flag + the current state-machine label so the
        UI can populate a fresh component without waiting for a
        notification.
        """
        per = self._decks[deck_id]
        return {
            "deck": deck_id.value,
            "enabled": per.enabled,
            "status": per.status.value,
            "seconds_to_mix": per.last_seconds_to_mix,
        }

    # ------------------------------------------------------------------
    # tick() — driven from the run_with_proposer loop's on_state handler
    # ------------------------------------------------------------------

    async def tick(self, state: EngineState) -> None:
        """Advance the state machine one notification's worth.

        Called from ``CoPilotService.run_with_proposer``'s on_state
        handler, immediately after the proposer's hysteresis filter has
        had its turn. The proposer already knows how to *pick* the next
        track; auto-mix decides whether to *execute* it.
        """
        for deck_id in (DeckId.A, DeckId.B):
            per = self._decks[deck_id]
            deck = state.deck(deck_id)
            seconds_to_mix = self._seconds_to_mix(deck)

            # Completed-transition check — once the loaded track changes
            # we know the previous transition fired and the deck is now
            # IDLE w.r.t. future windows.
            if per.status in (AutoMixStatus.TRANSITIONING, AutoMixStatus.DONE):
                loaded_id = deck.loaded.id if deck.loaded is not None else None
                if loaded_id != per.last_track_id:
                    per.status = AutoMixStatus.IDLE
                    per.last_track_id = None
                    per.last_seconds_to_mix = None
                    await self._notify(deck_id, per.status, None)
                continue

            if not self._eligible(deck, per):
                if per.last_seconds_to_mix is not None:
                    # Track restarted / scrubbed back / paused — reset
                    # countdown so the UI clears the indicator.
                    per.last_seconds_to_mix = None
                    await self._notify(deck_id, per.status, None)
                continue

            # We're inside the look-ahead window with auto-mix on.
            # Surface the countdown immediately even before we propose
            # — gives the UI something to animate.
            if seconds_to_mix != per.last_seconds_to_mix:
                per.last_seconds_to_mix = seconds_to_mix
                await self._notify(deck_id, per.status, seconds_to_mix)

            if per.status == AutoMixStatus.IDLE:
                proposal = self._proposer.on_state(state)
                if proposal is None:
                    # Library has nothing compatible — leave IDLE so a
                    # later tick can try again (library may grow mid-set).
                    continue
                per.status = AutoMixStatus.ARMED
                per.last_track_id = (
                    deck.loaded.id if deck.loaded is not None else None
                )
                await self._notify(deck_id, per.status, seconds_to_mix)
                # Kick off the actual event submission as a background
                # task so a slow engine doesn't block other decks' ticks.
                task = asyncio.create_task(
                    self._execute(deck_id, proposal),
                    name=f"auto-mix-{deck_id.value}",
                )
                per.in_flight_task = task

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _eligible(self, deck, per: _DeckAutoMixState) -> bool:  # type: ignore[no-untyped-def]
        """All preconditions for arming a transition on ``deck``."""
        if not per.enabled:
            return False
        if not deck.copilot_engaged:
            return False
        if not deck.playing or deck.loaded is None:
            return False
        seconds = self._seconds_to_mix(deck)
        return seconds is not None and seconds >= 0

    def _seconds_to_mix(self, deck) -> Optional[int]:  # type: ignore[no-untyped-def]
        """Whole seconds remaining until the look-ahead trigger fires.

        Returns ``None`` when the deck has no loaded track, no positive
        duration, or hasn't entered the look-ahead window yet. The
        return value drives the UI countdown — so ``0`` means "fires on
        the next tick".
        """
        if deck.loaded is None or not deck.playing:
            return None
        duration_ms = self._duration_ms(deck)
        if duration_ms is None or duration_ms <= 0:
            return None
        remaining_ms = duration_ms - deck.position_ms
        if remaining_ms > self._lookahead_ms:
            return None
        # Whole seconds, floored at 0 — a negative value would imply the
        # track has already overshot (engine clock drift). Treating it
        # as 0 keeps the UI clean.
        return int(max(0, remaining_ms // 1000))

    def _duration_ms(self, deck) -> Optional[int]:  # type: ignore[no-untyped-def]
        """Resolve duration via the proposer's library reference.

        The engine state carries position but NOT duration — that field
        lives on the library row. We reach through the proposer's
        library handle so the controller doesn't need its own.
        """
        if deck.loaded is None:
            return None
        ref = self._proposer._library.get(deck.loaded.id)  # noqa: SLF001
        if ref is None:
            return None
        return int(ref.duration_s * 1000)

    async def _execute(self, deck_id: DeckId, proposal: Proposal) -> None:
        """Submit every event in the proposal's plan.

        Advances ARMED → TRANSITIONING just before the first submit, then
        TRANSITIONING → DONE after the last. Cancellation (operator
        flipped auto-mix off mid-flight) leaves the state machine at
        IDLE — :meth:`set_auto_mix` does that reset synchronously, so
        we just have to swallow the :class:`asyncio.CancelledError`.
        """
        per = self._decks[deck_id]
        try:
            per.status = AutoMixStatus.TRANSITIONING
            await self._notify(deck_id, per.status, per.last_seconds_to_mix)
            for ev in proposal.events:
                await self._submit_event(ev)
            per.status = AutoMixStatus.DONE
            await self._notify(deck_id, per.status, None)
        except asyncio.CancelledError:
            log.info(
                "auto-mix on deck %s cancelled mid-transition", deck_id.value
            )
            # set_auto_mix already reset to IDLE + notified.
            raise
        except Exception:  # noqa: BLE001 — keep service alive
            log.exception(
                "auto-mix execution on deck %s raised; resetting to IDLE",
                deck_id.value,
            )
            per.status = AutoMixStatus.IDLE
            per.last_seconds_to_mix = None
            await self._notify(deck_id, per.status, None)
        finally:
            per.in_flight_task = None

    async def _notify(
        self,
        deck_id: DeckId,
        status: AutoMixStatus,
        seconds_to_mix: Optional[int],
    ) -> None:
        """Fan-out to the registered ``state_changed`` callable.

        Best-effort: a handler that raises must not take the controller
        down (proposer + auto-mix are independent of UI presence).
        """
        if self._state_changed is None:
            return
        try:
            await self._state_changed(deck_id, status, seconds_to_mix)
        except Exception:  # noqa: BLE001
            log.exception("auto-mix state_changed handler raised; swallowing")

    def _schedule_notify(
        self,
        deck_id: DeckId,
        status: AutoMixStatus,
        seconds_to_mix: Optional[int],
    ) -> None:
        """Sync entry-point that schedules an async notify if a loop runs.

        :meth:`set_auto_mix` is sync (called from RPC dispatch), but
        the state_changed notifier is async. We schedule the fan-out as
        a task when there's a running loop; otherwise we silently drop
        it (the next ``tick`` will pick the change up regardless).
        """
        if self._state_changed is None:
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return
        loop.create_task(self._notify(deck_id, status, seconds_to_mix))


__all__ = [
    "AutoMixController",
    "AutoMixStatus",
    "DEFAULT_LOOKAHEAD_MS",
    "StateChangedFn",
    "SubmitEventFn",
]
