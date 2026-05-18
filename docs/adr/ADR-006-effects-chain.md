# ADR-006 — Effects chain extensibility

## Status

Accepted. v0.1 effects (Filter, Echo, Reverb, Gate) implemented under
`engine/src/audio/effects/`.

## Context

Live DJ players need per-deck creative effects — filters, delays,
reverbs, beat-synced gating. The chain has to be:

1. **Audio-thread safe** — `process()` must not allocate, must not lock,
   must not block. (ADR-004 hard rule.)
2. **Per-deck**, 3 slots, applied in slot order.
3. **Hot-swappable** — assigning a different effect to a slot must not
   stall the audio thread.
4. **UI-discoverable** — the frontend needs a manifest of available
   effects + their parameters.

## Decision

### Topology

Each deck owns `[EffectSlot; 3]` (already in `state.rs`). The audio
thread holds a mirrored `[FxBank; 3]` per deck. A `FxBank`
**pre-allocates every built-in effect** so that hot-swapping a slot
just re-points an `effect_id` and `reset()`s the target — no
construction on the audio thread.

Slots run serially (slot 0 → slot 1 → slot 2). The output of slot N
feeds slot N+1; the final post-slot-2 buffer is the deck's signal
into the crossfader.

### Built-ins (v0.1)

| id | name    | params (descriptor order)                                         |
|----|---------|-------------------------------------------------------------------|
| 1  | filter  | `cutoff_hz` `resonance` `mode` (RBJ biquad LP/HP/BP)              |
| 2  | echo    | `time_ms` `feedback` `tone` (delay line + cross-fb + tone tilt)   |
| 3  | reverb  | `room_size` `damping` `width` (Schroeder 4-comb + 2-allpass)      |
| 4  | gate    | `period_div` `duty` (beat-synced from master clock + master BPM)  |

Id `0` is reserved for the empty slot.

### Param plumbing

The `EngineState` event log keeps the user-friendly form
(`EffectParam { param: String, value: f32 }`) so the audit trail
remains human-readable. The translator resolves the string into a
numeric index by asking `effects::resolve_param(effect_id, name)`
**on the control thread**, then emits a pure-POD
`AudioCommandKind::EffectParam { param_id: u8, value: f32 }`. The
audio thread never sees a `String`.

### Trait

```rust
pub trait Effect: Send + Sync {
    fn id(&self) -> EffectId;
    fn name(&self) -> &'static str;
    fn params(&self) -> &'static [ParamDescriptor];
    fn process(&mut self, buf: &mut [f32], params: &EffectParams,
               wet_dry: f32, sample_rate: u32);  // NO alloc
    fn reset(&mut self);
}
```

`EffectParams = [f32; 6]` — fixed-size key table. Each effect maps
descriptor index → param value. 6 is the per-effect cap.

### Buffer ownership

`process()` takes an `&mut [f32]` containing **interleaved stereo** (L,
R, L, R, …). Effects render in place. The mixer's stereo scratch
buffer flows: oscillator/decoder → per-deck stereo scratch → effects
chain → downmix to mono → crossfade → output. (Output is mono until a
separate stereo PR; the effects already run on stereo so that PR is
purely the master path.)

### Manifest endpoint

`engine.list_effects` JSON-RPC method returns `[ {id, name, params:
[descriptor…]}, … ]`. Stubbed to `[]` for v0.1; the static descriptor
data already lives in `effects::descriptors()` and will be marshalled
in a follow-up.

## Consequences

* No `unsafe`; pre-allocation strategy avoids the alloc-on-swap pitfall.
* Each `FxBank` holds all 4 effect structs whether assigned or not —
  the bulk of that is Echo's 2s × 96 kHz × stereo delay line (~1.5 MB).
  6 banks total (3 slots × 2 decks) → ~9 MB resident. Acceptable for
  a desktop DJ player; revisit if we add many more delay-line effects.
* Adding a new effect = implement `Effect` + add to `FxBank` +
  `descriptors()` + the `match` in `process()`. No registry indirection
  on the audio path.

## Open questions

* `engine.list_effects` returns `[]` until the static-descriptor
  marshaller lands.
* Master BPM update flow → Gate currently uses the BPM passed at
  `FxBank::new()`. Live BPM change requires a small audio command
  (covered separately by ADR-007 clock-sync).

## Addendum (2026-05-18) — Gate now reads live BPM

The Gate effect no longer caches `master_bpm` on the struct. Each
`process()` call reads `SharedClock::master_bpm()` (single
`AtomicU32` load with `Relaxed` ordering, no heap, no lock).

This means `EventKind::SetMasterBpm` — which the engine main loop
already forwards to the `SharedClock` side-channel — takes effect
at the next audio-buffer boundary (≤ ~10.7 ms at 1024-frame
buffers, 48 kHz) without any explicit effect-param command plumbing.
No new audio commands were added; the existing clock side-channel
from ADR-007 is the propagation path.

The `Gate::new(clock, sample_rate, master_bpm)` signature is kept
for backwards compatibility, but the `master_bpm` parameter is no
longer read — the `SharedClock` holds the canonical value.
