"""Pydantic mirrors of the Rust engine's serde shape (see ``engine/src/state.rs``).

These models exist for **two reasons**:

1. JSON-RPC envelope validation — when the engine pushes ``engine.state_changed``
   we want to reject malformed payloads at the boundary instead of trusting
   ``dict[str, Any]`` shaped data deep inside the decision functions.
2. A typed surface the decision functions can program against without dragging
   a Rust-Python FFI in. The Rust side is the source of truth; if a field is
   renamed there, the Python deserializer fails loudly and CI catches the
   drift.

The shapes intentionally mirror the serde-derived JSON shape exactly:
* ``DeckId`` serializes as ``"A"`` / ``"B"`` (serde tag for unit enums).
* ``EventKind`` is a tagged union via pydantic's ``discriminator='kind'`` pattern.
  The Rust side emits ``{"DeckPlay": {"deck": "A"}}`` (serde's default external
  tagging). We accept that envelope and unwrap it.
"""
from __future__ import annotations

from enum import Enum
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, field_validator


class DeckId(str, Enum):
    A = "A"
    B = "B"


class EqBand(str, Enum):
    Low = "Low"
    Mid = "Mid"
    High = "High"


class TrackRef(BaseModel):
    """Mirrors ``state::TrackRef``. ``id`` is library-stable; ``path`` is local FS.

    Hot-cue persistence PR: ``hot_cues`` is a library-side metadata
    extension that the engine doesn't store on its own ``state::TrackRef``
    (the engine carries it on ``EventKind::DeckLoad`` + ``Deck`` instead).
    It's mirrored here so the co-pilot's typed ``library.*`` responses
    have a Pydantic model to validate against — the engine's wire shape
    accepts the field via ``extra="ignore"`` on ``Deck`` and the
    externally-tagged ``DeckLoad`` payload.
    """

    model_config = ConfigDict(frozen=True, extra="ignore")

    id: str
    path: str
    # 8-slot hot-cue grid; each slot is either a ms-position (int >= 0)
    # or None when unset. Defaults to all-None so existing call sites
    # that only pass id/path continue to parse cleanly.
    hot_cues: list[int | None] = Field(
        default_factory=lambda: [None] * 8  # type: ignore[arg-type]
    )

    @field_validator("hot_cues")
    @classmethod
    def _exactly_eight_slots(
        cls, v: list[int | None]
    ) -> list[int | None]:
        if len(v) != 8:
            raise ValueError(
                f"hot_cues must have exactly 8 slots, got {len(v)}"
            )
        return v


class EffectSlot(BaseModel):
    model_config = ConfigDict(frozen=False)

    effect_id: int = 0
    params: dict[str, float] = Field(default_factory=dict)
    wet_dry: float = 0.0
    enabled: bool = False


class Deck(BaseModel):
    """Mirrors ``state::Deck``. Field names match serde-renamed JSON exactly."""

    model_config = ConfigDict(extra="ignore")

    loaded: TrackRef | None = None
    playing: bool = False
    position_ms: int = 0
    pitch_semitones: float = 0.0
    # Independent tempo control (pitch/tempo-independent PR). Mirrors
    # `Deck::tempo_ratio` in engine/src/state.rs. 1.0 = original speed,
    # clamped engine-side to [0.5, 2.0]. Pydantic `extra=ignore` means
    # older engine payloads without this field still parse cleanly.
    tempo_ratio: float = 1.0
    eq_low_db: float = 0.0
    eq_mid_db: float = 0.0
    eq_high_db: float = 0.0
    loop_in_ms: int | None = None
    loop_out_ms: int | None = None
    loop_active: bool = False
    hot_cues: list[int | None] = Field(default_factory=lambda: [None] * 8)
    copilot_engaged: bool = False
    bpm: float = 0.0
    beat_grid_anchor_ms: int = 0
    beat_period_ms: float = 0.0
    phase_offset_ms: int = 0
    # Per-deck downbeat grid (ms positions). Mirrors the engine's
    # `Deck::downbeats: SmallVec<[u32; 64]>`. Engine serializes with
    # `skip_serializing_if = SmallVec::is_empty` so notifications for
    # tracks without a downbeat grid omit the field entirely; pydantic
    # default = empty list covers that.
    downbeats: list[int] = Field(default_factory=list)
    effects: list[EffectSlot] = Field(default_factory=lambda: [EffectSlot() for _ in range(3)])
    handoff_until_frame: int = 0

    @field_validator("hot_cues")
    @classmethod
    def _exactly_eight_hot_cues(cls, v: list[int | None]) -> list[int | None]:
        if len(v) != 8:
            raise ValueError(f"hot_cues must have exactly 8 slots, got {len(v)}")
        return v


class EngineState(BaseModel):
    """Mirrors ``state::EngineState``. Tolerates unknown fields so the engine can
    add new state without breaking the co-pilot (forward compat)."""

    model_config = ConfigDict(extra="ignore")

    deck_a: Deck = Field(default_factory=Deck)
    deck_b: Deck = Field(default_factory=Deck)
    crossfader: float = 0.5
    master_volume_db: float = 0.0
    session_active: bool = False

    def deck(self, deck_id: DeckId) -> Deck:
        return self.deck_a if deck_id == DeckId.A else self.deck_b

    def other_deck(self, deck_id: DeckId) -> tuple[DeckId, Deck]:
        if deck_id == DeckId.A:
            return DeckId.B, self.deck_b
        return DeckId.A, self.deck_a


# ---------- Events ----------
# We don't need a full tagged-union over every EventKind — the co-pilot only
# *emits* events (it doesn't apply them locally; the engine does). So we model
# Event as a thin envelope and let EventKind be a small set of dataclass-style
# shapes we know how to construct.


class EventSource(str, Enum):
    Ui = "Ui"
    Copilot = "Copilot"


class _EventKindBase(BaseModel):
    model_config = ConfigDict(frozen=True)


class CopilotEngage(_EventKindBase):
    kind: Literal["CopilotEngage"] = "CopilotEngage"
    deck: DeckId


class CopilotDisengage(_EventKindBase):
    kind: Literal["CopilotDisengage"] = "CopilotDisengage"
    deck: DeckId


class DeckLoad(_EventKindBase):
    kind: Literal["DeckLoad"] = "DeckLoad"
    deck: DeckId
    track: TrackRef
    bpm: float
    beat_grid_anchor_ms: int
    # Downbeat positions in ms. Engine's serde default = [] when the
    # field is omitted, so this defaults to the empty list to keep
    # callers that haven't migrated yet wire-compatible. Field name
    # matches the Rust serde naming (snake_case).
    downbeats_ms: list[int] = Field(default_factory=list)
    # 8-slot hot-cue grid (hot-cue persistence PR). Mirrors the engine's
    # `EventKind::DeckLoad.hot_cues: [Option<u64>; 8]`. Default = all
    # None so a pre-PR DeckLoad emit still validates cleanly; the
    # engine fills the same default via `#[serde(default)]`.
    hot_cues: list[int | None] = Field(
        default_factory=lambda: [None] * 8  # type: ignore[arg-type]
    )

    @field_validator("hot_cues")
    @classmethod
    def _exactly_eight_slots(
        cls, v: list[int | None]
    ) -> list[int | None]:
        if len(v) != 8:
            raise ValueError(
                f"hot_cues must have exactly 8 slots, got {len(v)}"
            )
        return v


class DeckPlay(_EventKindBase):
    kind: Literal["DeckPlay"] = "DeckPlay"
    deck: DeckId


class LoopIn(_EventKindBase):
    kind: Literal["LoopIn"] = "LoopIn"
    deck: DeckId


class LoopOut(_EventKindBase):
    kind: Literal["LoopOut"] = "LoopOut"
    deck: DeckId


class CrossfaderRamp(_EventKindBase):
    """Not a literal engine event today — represented on the wire as a sequence
    of ``Crossfader`` events scheduled by the engine. The co-pilot describes
    intent here; the engine's RPC handler is responsible for scheduling the
    discrete ``Crossfader`` events at the requested phrase boundary.

    Kept distinct from ``CrossfaderSet`` so future engine versions can emit a
    smoother envelope (e.g. equal-power) instead of linear interpolation.
    """

    kind: Literal["CrossfaderRamp"] = "CrossfaderRamp"
    from_value: float = Field(ge=0.0, le=1.0)
    to_value: float = Field(ge=0.0, le=1.0)
    duration_bars: int = Field(gt=0, le=64)
    start_at_phrase_boundary: bool = True


EventKind = (
    CopilotEngage
    | CopilotDisengage
    | DeckLoad
    | DeckPlay
    | LoopIn
    | LoopOut
    | CrossfaderRamp
)


class Event(BaseModel):
    """Envelope mirroring ``state::Event``. ``id`` and ``ts_micros`` are filled
    by the engine on receipt — co-pilot leaves them 0 in outbound events."""

    model_config = ConfigDict(frozen=True)

    id: int = 0
    ts_micros: int = 0
    source: EventSource = EventSource.Copilot
    kind: EventKind


# ---------- JSON-RPC envelopes ----------


class JsonRpcRequest(BaseModel):
    """Outbound JSON-RPC 2.0 request envelope."""

    jsonrpc: Literal["2.0"] = "2.0"
    id: int | str
    method: str
    params: dict[str, Any] | list[Any] = Field(default_factory=dict)


class JsonRpcNotification(BaseModel):
    """Inbound notification (no ``id`` field per JSON-RPC 2.0)."""

    jsonrpc: Literal["2.0"] = "2.0"
    method: str
    params: dict[str, Any] = Field(default_factory=dict)


class StateChangedParams(BaseModel):
    """Params of the ``engine.state_changed`` notification.

    The engine pushes the full state after every reducer call. v0.1 is naive
    (full snapshot per change); a future PR will switch to a delta+sequence
    protocol once the engine ships an event-replay endpoint.
    """

    state: EngineState
    last_event_id: int | None = None
