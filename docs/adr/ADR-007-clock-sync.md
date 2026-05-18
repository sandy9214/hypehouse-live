# ADR-007 — External clock sync (Ableton Link, MIDI clock)

**Status**: Accepted 2026-05-17 — scope: v0.1 = MIDI clock OUT only (**implemented** — see `engine/src/midi/clock_out.rs`); v0.2+ = Ableton Link.
**Decider**: Sandeep Gorai
**Trigger**: Council review flagged this as a long-term refactor risk if punted (Cohere).

## Context

A live DJ rig typically syncs to / from other devices:

- **MIDI clock**: 24 PPQN tempo + start/stop. Universal, every DAW / VST / hardware drum machine speaks it.
- **Ableton Link**: peer-to-peer beat sync over LAN. Lower latency than MIDI clock; what modern DJs + producers use to sync their phone, laptop, MPC.
- **Pioneer ProDJ Link**: proprietary; CDJ-to-CDJ. Skip for v0.x.

If we ship without ANY external sync support, integrating later means re-doing the engine's notion of "what's the canonical tempo" since right now the per-deck `pitch_semitones` is the only authority.

## Decision

**v0.1**: MIDI clock OUT only. The engine has a single master tempo (derived from Deck A's BPM by default, configurable). It sends MIDI clock messages to a configured MIDI output device so the user's hardware drum machines + synths can lock to it.

**v0.2+**: Ableton Link IN + OUT. The engine joins the Link session on the LAN; tempo becomes a shared peer-to-peer value. The engine's master tempo and Link tempo cross-update (with a configurable "master role" toggle to prevent oscillation).

**v0.3+**: MIDI clock IN — accept external master clock from a hardware sequencer / DAW.

## Why this order

- MIDI clock OUT is the simplest of the three (we're the master, just emit). Validates the "engine has a master tempo" abstraction without external dependencies.
- Ableton Link adds peer discovery + LAN multicast — heavier infra, but unlocks the iPad-DJ workflow that many modern DJs use.
- MIDI clock IN is rarely used in DJ setups (the DJ IS the master) and lands last.

## EngineClock shape

```rust
pub struct EngineClock {
    pub sample_rate: u32,        // audio device rate
    pub frame: u64,              // absolute samples since session start
    pub master_bpm: f32,         // canonical session BPM
    pub master_phase: f32,       // [0.0, 1.0) within the current beat
    pub source: ClockSource,
}

pub enum ClockSource {
    Internal { anchor_deck: Option<DeckId> },
    AbletonLink,
    MidiClockIn { device: String },
}
```

Every audio buffer, the engine advances `frame` by buffer-size + recomputes `master_phase` from `master_bpm + frame + sample_rate`. MIDI clock OUT fires 24 PPQN based on this; Ableton Link (v0.2) syncs `master_bpm` + `master_phase` against peers.

## Open questions

- Initial `master_bpm`: 120.0 default. User can override via UI. When Deck A loads a track and `ClockSource::Internal { anchor_deck: Some(A) }`, sync master_bpm to that track's BPM.
- MIDI clock OUT device selection: env var `MIDI_CLOCK_OUT_DEVICE=...` for v0.1; UI selector later.
- Tempo nudge precision: MIDI clock spec accepts integer 24-PPQN ticks. For sub-BPM nudges we cheat by sending ticks slightly early/late — acceptable jitter for downstream gear.

## What v0.1 ships

Stub `EngineClock` with `ClockSource::Internal` only. MIDI clock OUT module behind a feature flag, default off. Real wiring lands in a v0.1.x PR after audio thread is in place.

### v0.1 implementation notes (PR `engine-midi-clock-out-v01`)

The MIDI clock OUT module landed against `main` with the following shape:

* **Module**: [`engine/src/midi/clock_out.rs`](../../engine/src/midi/clock_out.rs) — `MidiClockOut::start(Some("device-substring"), shared_clock)` opens a `midir::MidiOutput` port (substring match, case-insensitive), spawns a `std::thread` named `hypehouse-midi-clock-out`, and emits MIDI **Start** (0xFA) once, then **Clock** (0xF8) at 24 PPQN derived from `SharedClock::master_bpm()` (re-read every tick so `SetMasterBpm` propagates within one tick).
* **Shutdown**: `Drop` on the handle sets a cancellation flag, joins the worker (≤1 tick period), and emits **Stop** (0xFC).
* **Configuration**: env var `MIDI_CLOCK_OUT_DEVICE` selects the port. Empty / unset = disabled. Cargo feature `midi-clock-out` gates compilation of the `midir` output binding — default off.
* **Tempo source**: `EventKind::SetMasterBpm { bpm: f32 }` updates `EngineState::master_bpm` (validated f32) AND mirrors into `SharedClock` via a side-channel atomic (`AtomicU32` storing `f32::to_bits`). The audio thread does not consume `SetMasterBpm` — only the MIDI clock OUT (and, in v0.2, Ableton Link) reads `SharedClock::master_bpm()`.
* **Anchor deck**: not yet wired. `ClockSource::Internal { anchor_deck: None }` is implied. v0.1.x will add an `EventKind::SetClockAnchor { deck: Option<DeckId> }` and the control loop will mirror that deck's BPM on `DeckLoad` / `PitchBend`.
* **Timing**: ~10 ms `thread::sleep` + spin-yield tail. Measured ~35 µs mean / 400 µs stddev / ~6 ms max jitter on a 2024 M2 MacBook Air running a release build (Cargo also active). MIDI hardware tolerates ±5 ms; pure-spin opt-in is a v0.2 nice-to-have.
* **Tests**: 12 unit tests against an in-memory `MidiSink` trait — 24 PPQN @ 120 BPM, Start emission, BPM change reactivity, Drop emits Stop, substring port selection, bad-BPM clamp, 1 BPM idle (no busy loop).

## What v0.2+ unlocks

Full Link support via `rust-link` crate (community Rust binding to Ableton's Link C++ SDK). Requires Cargo feature `link` + accepting the Link license. UI gains a "Link" toggle + peer count badge.
