# Session recording — master mix WAV

**Status**: v0.1
**Owner**: engine crate, module `recording` (`engine/src/recording/mod.rs`)
**Related**: ADR-003 (event-sourced state), ADR-004 (audio thread rules).

The engine records the final master mix of every session to a WAV file
so a DJ can listen back to (or share / edit) their set after the fact.
The recorder lives alongside the audio mixer; the audio thread tees
each rendered chunk into a lock-free SPSC ring, and a dedicated writer
thread drains the ring to disk.

## On-disk layout

```text
$HYPEHOUSE_EVENT_LOG_DIR or
$XDG_DATA_HOME/hypehouse-live/sessions or
~/.local/share/hypehouse-live/sessions
    20260518T013312Z-a4f2/
        events.jsonl   ← persistence::EventLog (ADR-003)
        master.wav     ← recording::MasterRecorder
```

The session id is the same one used by the event log so the two files
sit side by side; replaying the event log against the master mix
reconstructs the full session timeline + audio for debugging.

## WAV format

* PCM IEEE float (format = 3)
* 32-bit, stereo (the engine's mono mix is duplicated to L = R)
* Sample rate = audio device's preferred rate (typically 48 000 Hz)
* 44-byte header (`RIFF` / `WAVE` / `fmt ` / `data` chunks)
* `RIFF` and `data` size fields are written as placeholder zeros at
  open time, then patched in `stop()` once the body byte count is
  known. `fsync` runs after the patch.

Decoded by every common WAV reader (`hound`, `symphonia`, `ffmpeg`,
`afplay`, Audacity, Logic, Reaper, …).

## Configuration

| Env var                        | Effect                                                   |
|--------------------------------|----------------------------------------------------------|
| `HYPEHOUSE_RECORDING_DISABLED` | `=1` → no file created, no writer thread, no tee path.   |
| `HYPEHOUSE_EVENT_LOG_DIR`      | Overrides storage root; master.wav follows the event log.|
| `XDG_DATA_HOME`                | Standard XDG override.                                   |

A non-fatal open failure (perms, disk full) is logged at `warn` and
the engine continues without recording — the live set is never killed
by a recording bug.

## Audio-thread rules (ADR-004)

The push path is alloc-free, lock-free, syscall-free:

1. Materialise the chunk's mono mix into a pre-allocated stereo
   scratch (`AudioMixer::rec_scratch`, sized to the per-chunk pull).
2. `MasterRecorderSink::push(&slice)` calls `HeapProd::push_slice` —
   a memcpy + a release store.
3. On partial fit, one `fetch_add` on the drop counter (`Relaxed`).
   Recording continues; the writer catches up on the next drain.

Measured worst-case latency for a 1024-frame stereo block on x86_64
release builds: ≈ 2 µs (see
`recording::tests::push_latency_under_budget_1024_block`).

## Writer thread

* Drains the ring every 10 ms (or sooner if `stop()` is signalled).
* Uses a 64 KiB `BufWriter` so the syscall pressure is one
  `pwrite`/`write` per drain.
* Exits when `stop_flag` is set AND the ring is empty.

## Shutdown

```rust
shutdown_signal().await;
server.shutdown().await?;
drop(stream);                    // joins cpal audio thread
if let Some(rec) = master_recorder.as_mut() {
    rec.stop()?;                 // joins writer, patches header, fsync
}
```

`stop()` is idempotent — calling it twice will not double-patch the
header. The `Drop` impl on `MasterRecorder` runs `stop()` best-effort
if it wasn't already called, so a panicking shutdown still produces a
well-formed WAV up to the last drained chunk.

## Test coverage

Covered in `engine/src/recording/mod.rs::tests`:

* WAV header bytes match spec after stop (1000 samples).
* Round trip: 48 000 stereo f32 samples pushed → re-decoded →
  sample-for-sample equality.
* Ring overflow: push 4× ring capacity in one shot → dropped-frames
  counter increments + recording continues.
* Disabled mode: `HYPEHOUSE_RECORDING_DISABLED=1` → no file created.
* `stop()` is idempotent (no header corruption on the second call).
* `push()` is alloc-free under `assert_no_alloc`.
* Push latency on a 1024-frame stereo block stays under budget.
* Push after `stop()` does not panic (covers cpal-callback ordering
  during shutdown).
