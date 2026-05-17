# ADR-004 — Realtime audio thread contract

**Status**: Accepted 2026-05-17
**Decider**: Sandeep Gorai
**Trigger**: Council review of v0.1 scaffold flagged this as the single biggest pre-emptive risk (4/4 voices).

## Context

ADR-003 defines an event-sourced state log. Folding events is `O(events)` and uses `clone()` everywhere — fine for the control plane but lethal on the audio thread, which must service the soundcard's callback every ~2.6ms at 256-sample / 48kHz (or ~5.3ms at 256-sample / 96kHz). Any allocation, blocking I/O, or unbounded computation inside the cpal callback = audio underrun = audible pop / glitch / xrun, which is unacceptable for a live DJ player.

## Decision

Two threads + one **lock-free SPSC ring buffer** between them. The state-log + reducer + folding lives on the **control thread**. The audio output runs in the **realtime thread** which is owned by cpal and may NOT allocate or block. The control thread translates state changes into **sample-accurate commands** posted to the queue; the realtime thread pops + executes them at the boundary of each audio buffer.

```
┌─────────────────┐                   ┌──────────────────┐
│  Control thread │                   │  Realtime thread │
│                 │                   │  (cpal callback) │
│  Event log      │  ring buffer      │                  │
│  Reducer        │  ───SPSC───────►  │  Audio renderer  │
│  Diff → cmds    │  AudioCommand     │  Sample-accurate │
│                 │                   │  scheduler       │
└─────────────────┘                   └──────────────────┘
```

## AudioCommand shape (v0)

```rust
#[derive(Clone, Copy, Debug)]
pub struct AudioCommand {
    /// Apply at absolute sample frame (engine clock). Lets the control
    /// thread schedule events on the next beat / next bar without race.
    pub at_frame: u64,
    pub kind: AudioCommandKind,
}

#[derive(Clone, Copy, Debug)]
pub enum AudioCommandKind {
    DeckPlay { deck: DeckId },
    DeckPause { deck: DeckId },
    DeckSeek { deck: DeckId, frame: u64 },
    Crossfader { target: f32, ramp_frames: u32 }, // smooth-ramp, no zipper noise
    EqLow { deck: DeckId, target_db: f32, ramp_frames: u32 },
    EqMid { deck: DeckId, target_db: f32, ramp_frames: u32 },
    EqHigh { deck: DeckId, target_db: f32, ramp_frames: u32 },
    Pitch { deck: DeckId, semitones: f32, ramp_frames: u32 },
    LoopArm { deck: DeckId, in_frame: u64, out_frame: u64 },
    LoopDisarm { deck: DeckId },
    DeckLoadBuffer { deck: DeckId, buffer_id: u32 }, // buffer is pre-decoded on control thread
    DeckUnload { deck: DeckId },
}
```

Commands are `Copy + 'static` so they fit in a fixed-size ring slot. Audio buffers (`Arc<DecodedTrack>`) live in a separate `Arc`-mapped registry; the audio thread reads them via lock-free index — never allocates.

## Hard rules on the audio thread

The cpal callback must NEVER:

- `Box::new`, `Vec::push` past capacity, `String::from`, `HashMap::insert`, or any heap allocation.
- Lock a `Mutex`, `RwLock`, or any blocking primitive. Lock-free only.
- Call into FFI that may allocate (audio decoders run on the control thread; audio thread only consumes pre-decoded `f32` buffers).
- Call `println!`, file I/O, or anything that can block.
- Run for longer than ~50% of its budget (i.e., for a 5.3ms budget, max 2.6ms wall-time).

Enforce via:
- `#[forbid(unsafe_code)]` everywhere except a single audited `audio_io` module.
- A static analyzer pass (`clippy::all` + a `audio-thread-purity` lint we'll write or use the `assert_no_alloc` crate).
- A criterion benchmark in CI that asserts the callback p99 < 50% budget on a representative workload.

## Why not put the reducer on the audio thread?

Tempting (single source of truth) but fatal: every event clone is a heap allocation. Event log + reducer must stay off-realtime.

## Why ring buffer not channel?

Standard `std::sync::mpsc` uses a `Mutex` under the hood + heap-allocates messages. `ringbuf` crate is lock-free SPSC, fixed capacity, zero allocation per push/pop.

## State-log → command translation

The control thread, on every event applied:

1. Diff `prev_state` vs `new_state`.
2. Generate one or more `AudioCommand`s with appropriate `at_frame` (often "now" = next buffer boundary, sometimes "next beat" for beat-aligned transitions).
3. Push commands into the ring buffer.

The audio thread, on every callback:

1. Read the current engine clock (sample counter).
2. Drain the ring buffer up to `(at_frame ≤ end_of_this_buffer)`.
3. Update internal hot state (gain ramps, crossfader, EQ state, deck playheads).
4. Render samples.

## Open implementation questions

- Ring buffer capacity: 1024 commands per direction should suffice (a busy live set is ~10 events/sec; 1024 = ~100s of buffering). Bench under MIDI flood.
- Engine clock source: cpal's `OutputCallbackInfo.timestamp.callback` (monotonic sample frame at callback start). Trust it.
- How fast can a fresh `DecodedTrack` (e.g. 6-minute mp3) be ready post-`DeckLoad`? Decode on control thread + fire `DeckLoadBuffer { buffer_id }` when done. Target <500ms for a typical track on M-series hardware.
