# hypehouse-live

**AI-augmented DJ tool — software-only, open source.**

Rust audio engine + Tauri/WebSocket bridge + TypeScript UI + Python AI co-pilot.
Target: bedroom DJs / livestreamers / mixtape creators + AI auto-mix workflows. No DJ controller hardware required — runs on a laptop with keyboard + mouse. Optional WebMIDI in browser if a controller IS present.

Closest peer: **Mixxx** (open-source DJ software). Differentiator: AI co-pilot with mashability-scored auto-mix + stem-aware mixing + LUFS-normalized library + cloud-shareable preset snapshots + crowd-pleaser export.

## Quick map

| Path | Purpose | Stack |
|---|---|---|
| `engine/` | Rust core: 2-deck audio engine, crossfader, time-stretch, MIDI input | `cpal` `symphonia` `rubato` `midir` |
| `ui/` | TypeScript frontend: 2 decks + crossfader + library browser + co-pilot toggle | Vite + TS + Web Audio API (for visualization) |
| `copilot/` | Python service: AI track selection + auto-mix decisions; consumes the carved-out `dj_engine` analysis from HypeHouse v1 | Python 3.11, librosa, madmom, mashup scoring |
| `docs/adr/` | Architecture decision records — what we're building, why, what we rejected | Markdown |
| `prompts/` | LLM prompt templates (council review, AI-DJ decisions) | `*.prompt.yaml` |
| `scripts/` | Dev tooling, deploy, CI helpers | Shell |

## Co-pilot vs manual

| Mode | Who's driving | Auto-mix transitions | Drop-in / take-over |
|---|---|---|---|
| **Manual** | User + MIDI controller | No | n/a |
| **Co-pilot** | AI picks next track + executes phrase-aligned transitions | Yes | User can take over with no audio glitch (deck handoff) |
| **Hybrid** | User manual on deck A; AI co-pilot on deck B | Per-deck independent | Default for hardware sets |

## Features

- 2-deck live mixing engine (`cpal` + `symphonia` + `rubato`), MIDI input via `midir`
- Event-sourced state log (ADR-003) — replayable session for live-set debugging
- Effects chain v0.1 — filter / echo / reverb / gate (ADR-006)
- WebSocket + JSON-RPC bridge with bearer-token auth (browser + native modes)
- **MIDI clock OUT (alpha, ADR-007 v0.1)** — engine acts as master, emits 24 PPQN to hardware drum machines / synths. Enable with `cargo run --features midi-clock-out` and `MIDI_CLOCK_OUT_DEVICE=<substring>`. See [`docs/api/ws-protocol.md`](docs/api/ws-protocol.md#midi-clock-out-adr-007-v01).
- **MIDI clock IN (alpha, ADR-007 v0.3)** — engine acts as slave, locks `master_bpm` to an external sequencer / DAW via 24 PPQN MIDI clock with 4-beat smoothing + ±0.1 BPM deadband. Enable with `cargo run --features midi-clock-in` and `MIDI_CLOCK_IN_DEVICE=<substring>`. When IN is active, OUT is silently disabled to avoid feedback loops. See [`docs/api/ws-protocol.md`](docs/api/ws-protocol.md#midi-clock-in-adr-007-v03).
- Co-pilot service (Python): beat-grid + downbeat analysis, mashup scoring, next-track suggestions

## Status

- **Software-only positioning** (2026-05-18) — pivoted away from "drive your CDJ from a laptop" market. Target = Mixxx audience + AI auto-mix workflow. See [issue #93](https://github.com/sandy9214/hypehouse-live/issues/93) for v0.x caveats.
- v0.1 in development. v0.2 beta: 1-2 weeks (synthetic audio bake-in + Tauri unsigned binaries).
- v1.0 target: 3-4 months (streaming source + cloud library + marketing).
- v1 (mixtape compiler) remains live at github.com/sandy9214/HypeHouse — use that for batch render.

## Out of scope (v1)

- Pioneer ProDJ Link
- Vinyl mode / scratch emulation w/ hardware turntable controllers
- Native CDJ / DDJ hardware-specific timing — software-only with WebMIDI fallback only.

## Setup

(Pending — see `docs/setup.md` once initial scaffold lands)

## License

TBD
