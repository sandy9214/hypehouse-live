# ADR-001 — Stack choice: Rust engine + Tauri/WS bridge + TS UI + WebMIDI

**Status**: Accepted 2026-05-17
**Decider**: Sandeep Gorai

## Context

HypeHouse v1 (Python + Flask + librosa + render-to-mp3) hit a structural ceiling: the model is "compile a mixtape then stream the mp3". Cannot do live deck control, MIDI hardware input, sub-100ms audio response, or per-deck independent state. User wants v2 = **live DJ player**, not v1 incremental.

Three candidate stacks were evaluated:

| Option | Latency | Dev velocity | Controllers | Cross-platform |
|---|---|---|---|---|
| WebAudio-only (TS in browser) | 5–20ms | HIGH | WebMIDI native | Native (any modern browser) |
| **Rust core + TS UI via Tauri/WS** | **<5ms** | **LOW** (Rust learning curve) | **midir native** | Tauri ships installers; WS works in browser too |
| Python + Cython hot loops | 50–200ms | HIGH (reuse v1) | python-rtmidi clunky | Wheels per platform |

## Decision

Rust core + TS UI via Tauri (desktop) or WebSocket bridge (browser-only fallback). MIDI via `midir`. UI uses Web Audio API for visualization (waveforms, EQ meters) only — the audio path stays in Rust.

## Why

- **Latency floor matters for live DJ**. Pro hardware (Pioneer CDJ) targets <3ms button-to-audio. WebAudio-only sits at 5–20ms which is OK but not pro. Python is 50–200ms and disqualifies the live category outright.
- **DSP quality**: `rubato` (high-quality time-stretch) + `cpal` (lock-free audio thread) + native FFT crates outperform WebAudio's `AudioWorklet` for the heavy lifting.
- **Single audio thread + lock-free message passing** is the standard pro-audio architecture (used in Reaper, Bitwig, Renoise). Easy to express in Rust, awkward in browser-only.
- **Tauri** lets us ship a desktop binary AND keep the same TS UI working in a browser dev tab when convenient.
- **midir** gives us MIDI on macOS / Linux / Windows from a single Rust crate.

## Rejected alternatives

### WebAudio-only

Faster to ship a prototype but ceiling is real: cannot achieve <5ms reliably across browsers, AudioWorklet still has occasional GC pauses, and we'd be re-implementing in the browser things Rust crates already do better (time-stretch, MIDI sysex, sample-accurate scheduling). Saved for a future "browser-only fallback" build target.

### Python + Cython

The bottleneck isn't Python-vs-C speed — it's the entire render-then-play model that v1 inherited. Cython on the crossfade math would not move us from "render to mp3" to "live decks". Lateral move at best.

## Consequences

- **Build complexity** increases: Rust toolchain + Tauri's cross-platform packaging + TS frontend bundler all in one repo.
- **CI** needs Rust + Node matrices on Linux/macOS/Windows. Tradeoff accepted.
- **Co-pilot** service stays Python so HypeHouse v1's analyzer + mashup scoring can be vendored verbatim. Communicates with the Rust engine over JSON-RPC (local TCP or stdio).
- **Hot-reload** during dev: Rust engine recompiles in 2–5s; UI hot-reloads via Vite. Acceptable.

## Open questions for follow-up ADRs

- ADR-002: Deck primitive (2 vs 4; co-pilot semantics).
- ADR-003: Event-sourced state log shape.
- ADR-004: MIDI mapping format (JSON vs Lua vs custom DSL).
- ADR-005: Co-pilot RPC protocol.
