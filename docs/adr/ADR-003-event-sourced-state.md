# ADR-003 — Event-sourced state log

**Status**: Accepted 2026-05-17
**Decider**: Sandeep Gorai

## Context

HypeHouse v1's central data structure is `JOBS = {}`, a mutable dict mutated by 40+ call sites in web.py. Every architectural improvement landed today (durable JobStore, nested-flush mutation proxies, structured logging context) was a workaround for that single design choice. v2 must start with a state model that doesn't paint us into the same corner.

## Decision

Single append-only event log is the source of truth for live session state. Every UI action, MIDI input, and co-pilot decision generates an event. The engine state is a fold over the log.

```
UI/MIDI/Copilot → Event → Engine reducer → New state + Audio command
                    ↓
                  Event log (append-only)
```

## Why

- **Undo for free**: rewind = re-fold up to event N–k. DAW-grade undo, no extra code.
- **Live-set debugging**: full replay of the night. Crash at 2am? Replay the last 30 events on a dev machine and reproduce.
- **Multi-client sync** (future): the log is the wire format. Mobile remote control becomes "subscribe to log + emit events" rather than "two state machines fighting".
- **No shared mutable state** = no proxies, no flush guards, no JOBS-dict-with-40-mutators. The mutation problem we spent half of today fighting in v1 just doesn't exist.
- **Reasoning surface stays small**: every reviewer can understand "given event E and state S, the new state is S'". Pure function.

## Event shape (v0)

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Event {
    pub id: u64,            // monotonic per session
    pub ts_micros: i64,     // unix micros, engine clock
    pub source: EventSource, // Ui | Midi { device, mapping } | Copilot
    pub kind: EventKind,
}

pub enum EventKind {
    DeckLoad { deck: DeckId, track: TrackRef },
    DeckPlay { deck: DeckId },
    DeckCue { deck: DeckId, position_ms: u64 },
    Crossfader { value: f32 },         // [0, 1]
    EqAdjust { deck: DeckId, band: EqBand, value_db: f32 },
    HotCueSet { deck: DeckId, slot: u8, position_ms: u64 },
    HotCueTrigger { deck: DeckId, slot: u8 },
    LoopIn { deck: DeckId },
    LoopOut { deck: DeckId },
    LoopExit { deck: DeckId },
    PitchBend { deck: DeckId, semitones: f32 },
    CopilotEngage { deck: DeckId },
    CopilotDisengage { deck: DeckId },
    TakeOver { deck: DeckId },         // user pre-empts copilot
    SessionStart {},
    SessionEnd {},
}
```

## Storage

- In-memory ring buffer during the live session (last N=100k events).
- Periodic snapshot to disk (every 60s or 5MB delta, whichever first).
- Snapshot format: bincode-encoded full state + a pointer to the event-log offset it was taken from.
- Recovery: load latest snapshot + replay events past its offset.

## Why not a CRUD database

Postgres / sqlite for live audio events would add network latency to the hot path. Even sub-ms over a unix socket adds jitter. Ring buffer + disk snapshot keeps the engine purely local.

## Open implementation questions

- ADR-005 will define the co-pilot RPC: does the co-pilot return events directly into the log, or commands that the engine translates into events? Leaning toward events-direct so the audit trail is uniform.
