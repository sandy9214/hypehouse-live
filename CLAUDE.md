# hypehouse-live — Claude Code context

Team-shared context. AI DJ Player v2 — live mixing.

## Overview

**Software-only AI-augmented DJ tool.** Closest peer: Mixxx (open-source DJ software). Differentiator: AI co-pilot with mashability-scored auto-mix + stem-aware mixing + LUFS-normalized library + cloud-shareable preset snapshots + crowd-pleaser export.

Rust audio engine + Tauri/WebSocket bridge + TypeScript UI + Python AI co-pilot.

**Hardware out of scope** (pivot 2026-05-18): no Pioneer CDJ/DDJ-specific timing, no ProDJ Link, no vinyl-mode scratch hardware emulation. WebMIDI works as a fallback when a controller IS present (e.g. via a USB DDJ-200) but is NOT the primary input — keyboard + mouse + UI are. Target audience = bedroom DJs, livestreamers, mixtape creators, AI-assisted set automation.

Distinction from HypeHouse v1: v1 is a **mixtape compiler** (batch render to mp3, then play). v2 is a **software-only live tool** (real-time multi-deck + AI co-pilot + export-to-mixtape). Same audio-analysis core, totally different surface.

## Tech stack

- **Engine**: Rust (1.95+). `cpal` audio I/O, `symphonia` decode, `rubato` time-stretch, `midir` MIDI, `serde_json` IPC.
- **Bridge**: Tauri (desktop) OR WebSocket (browser-only mode).
- **UI**: TypeScript + Vite. Web Audio API for visualization only (waveforms, EQ meters); actual audio path stays in Rust.
- **Co-pilot**: Python 3.11. Imports analyzer/mashup primitives from HypeHouse v1 (vendored or pip-from-git).
- **MIDI**: Day-1 keyboard + Pioneer DDJ-200 mapping; user JSON config for others.

## Key commands

(Pending — populated after Cargo + Vite scaffolds land.)

## Architecture

- **Deck primitive**: 2 decks (A, B), each with `play / cue / pause / pitch / tempo / 3-band EQ / 8 hot cues / loop in-out / 4-8-16-bar auto-loop`.
- **Master**: crossfader, master volume, recording (to file).
- **State**: event-sourced log. Every UI/MIDI event → engine command → state delta + audio output. No mutable shared state.
- **AI co-pilot**: toggle per-deck. When on, the co-pilot service receives the upcoming track decision request via JSON-RPC and replies with `{next_track_id, transition_plan}`. Engine executes the plan; user can take over anytime via deck handoff (UI button + MIDI button).

## ADRs

See `docs/adr/`:
- ADR-001: Choose Rust + Tauri + WebMIDI
- ADR-002: 2-deck + co-pilot primitive
- ADR-003: Event-sourced state log

## Conventions (inherited from HypeHouse v1)

- Pre-commit hooks: `cargo fmt --check`, `cargo clippy`, `eslint`, `tsc --noEmit`, `ruff` on `copilot/`.
- CI: GitHub Actions matrix on Linux + macOS + Windows for the engine; Linux only for UI + co-pilot.
- Council merge gate: every PR needs cloud-quint (Codex + Gemini + Groq + GitHub-DeepSeek/Cohere + Claude) APPROVE.
- 4-min push spacing on main.
- No deploy without explicit user approval.

## Things Claude must NOT do

- Force-push to `main`.
- `cargo install` outside the project's pinned toolchain.
- Vendor `unsafe` Rust without an ADR.
- Bypass MIDI input validation (controllers can send arbitrary CCs; engine must clamp).
- Drop event-sourced log entries (audit trail for live-set debugging).
- Add background threads without a documented join+shutdown path.

## Carry-over from HypeHouse v1

Reusable verbatim:
- `src/analyzer.py` (BPM, key, beats, downbeats, energy)
- `src/mashup.py` (`mashability_score`, ranking)
- `src/shared_cache.py` (LocalCache + GcsCache)
- `prompts/council/*.prompt.yaml`

New surface:
- Rust engine (no analogue in v1)
- Multi-deck state machine
- MIDI input layer
- Tauri/WebSocket bridge
- Frontend UI
