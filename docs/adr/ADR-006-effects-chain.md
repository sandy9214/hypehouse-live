# ADR-006 — Effects chain extensibility

**Status**: Accepted 2026-05-17
**Decider**: Sandeep Gorai
**Trigger**: Council review flagged effects chain absence as a gap (Cohere).

## Context

Pro DJ apps ship effects: filter (low-pass / high-pass), echo / delay, reverb, flanger, bit-crusher, gate. Currently the Deck struct has 3-band EQ only. Adding effects later without a designed extension point would force a refactor.

## Decision

Each deck has an **effects chain**: an ordered list of effect slots (default 3 slots per deck). Each slot can be empty, or hold one effect with parameters. Effects are registered by name + a stable schema at engine boot; UI discovers available effects via JSON manifest.

```rust
pub struct EffectSlot {
    pub effect: Option<EffectId>,
    pub params: BTreeMap<String, f32>,   // per-effect parameter values
    pub wet_dry: f32,                    // 0.0 = dry, 1.0 = full wet
    pub enabled: bool,
}

pub struct Deck {
    // ... existing fields ...
    pub effects: [EffectSlot; 3],
}

pub type EffectId = u32;  // index into engine effect registry
```

Effect registry built into the audio engine:

```rust
pub struct EffectRegistry {
    by_id: HashMap<EffectId, Effect>,
}

pub trait Effect: Send + Sync {
    fn id(&self) -> EffectId;
    fn name(&self) -> &'static str;
    fn params(&self) -> &[ParamDescriptor];
    /// Process in-place; must not allocate.
    fn process(&self, buf: &mut [f32], params: &EffectParams, wet_dry: f32);
    /// Optional smooth ramp on param change (sample-accurate).
    fn ramp_param(&self, _name: &str, _from: f32, _to: f32, _frames: u32) {}
}
```

## v0.1 effects

Ship 4 built-in effects to validate the abstraction; everything else lands later:

| ID | Name | Params |
|---|---|---|
| 1 | `filter` | `cutoff_hz` (20–20000), `resonance` (0.0–1.0), `mode` (lowpass/highpass/bandpass) |
| 2 | `echo` | `time_ms` (10–2000), `feedback` (0.0–0.95), `tone` (-1.0 dark .. +1.0 bright) |
| 3 | `reverb` | `room_size` (0.0–1.0), `damping` (0.0–1.0), `width` (0.0–1.0) |
| 4 | `gate` | `period_div` (1/2 / 1/4 / 1/8 / 1/16 beat), `duty` (0.0–1.0) |

All four implementable in pure Rust with no FFI.

## Why not VST3 host?

VST3 SDK is C++ and would force us to load arbitrary native code with arbitrary realtime characteristics. Some VSTs DO allocate on the audio thread. Future ADR could revisit; for now, in-house effects keep the audio-thread purity guarantee.

## Why slot-based, not graph-based?

Slot chain is sufficient for DJ effects (effects are typically serial: filter → echo → reverb). Modular graphs are overkill and add UI complexity. Slot chain matches the mental model of Pioneer's beat-FX section + every DJ controller's FX bank.

## Events

Add to `EventKind`:

```
EffectAssign { deck, slot, effect_id }
EffectClear { deck, slot }
EffectParam { deck, slot, param_name, value }
EffectWetDry { deck, slot, value }
EffectEnable { deck, slot, enabled }
```

All clamping happens in the reducer per the effect's `ParamDescriptor` range.

## Open questions

- Sidechain routing (e.g., kick triggers gate): defer to ADR-008.
- Master-bus effects (effects applied to the post-mixer signal): defer; v0.1 is per-deck only.
