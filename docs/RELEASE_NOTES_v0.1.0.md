# hypehouse-live v0.1.0 — Initial public preview

**Software-only AI-augmented DJ tool. Closest peer: [Mixxx](https://mixxx.org).**

Rust audio engine + Tauri/WebSocket bridge + TypeScript UI + Python AI co-pilot.

## What it is

A laptop-only DJ tool with an AI co-pilot. Mix, transition, and live-stream sets
with mashability-scored auto-mix, stem-aware mixing, LUFS-normalized library, and
shareable preset snapshots.

**No DJ controller hardware required.** Runs on a laptop with keyboard + mouse +
UI. WebMIDI is optional — a controller works if you have one, but it's not the
primary input. Target audience: bedroom DJs, livestreamers, mixtape creators,
AI-assisted set automation.

## Headline features

- **Two-deck live mixing** with crossfader, EQ, hot cues, loop bar presets
- **AI co-pilot** with mashability-scored auto-mix + transition planning
- **Stem-aware mixing** — per-stem mute toggles (vocals/drums/bass/other)
- **LUFS-normalized library** — auto-leveled to -14 LUFS
- **Smart key transposer** — auto-pitch incoming deck to match active deck's key
- **Live MIDI clock IN/OUT** — sync to external DAWs / drum machines
- **Crowd-pleaser export** — auto-trim setlist record + chapter markers for upload
- **HTTP streaming source** — open remote audio URLs directly
- **SoundCloud streaming** — search + add to library
- **GitHub Pages demo** — auto-deployed UI preview on every main push
- **Cross-platform** — Linux + macOS + Windows engine; UI runs in any modern browser

## Differentiator from Mixxx

Mixxx is the open-source DJ software people compare to. hypehouse-live ships:
- AI auto-mix + transition planning (no analogue in Mixxx)
- LUFS-normalized library + auto-leveler
- Mashability-scored next-track suggestions
- Stem-aware mixing (out-of-the-box, not via plugin)
- Crowd-pleaser export (auto-edit highlight reel)
- Cloud-shareable preset snapshots (planned v0.2)

Mixxx ships hardware-controller integration we explicitly don't (Pioneer
ProDJ Link, vinyl mode, etc.). Different audience.

## What's NOT in v0.1.0

- **Tauri binaries are unsigned** — runtime "unknown developer" warning on
  macOS + Windows. Signing decision deferred until paid cert.
- **LFO modulation scaffold WIP** (#89) — not wired into effects.
- **Ableton Link real backend** — v0.2.x.
- **Cloud library sync** — v0.2 (#102).
- **Engine-level WS RPC + UI picker for virtual output device** — engine
  honours `HYPEHOUSE_OUTPUT_DEVICE` env var (#113), but UI picker shipping
  in follow-up PR.

See [docs/known-limitations.md](known-limitations.md) for the full
v0.x caveat list.

## Install

(Pending — binaries upload follows release tag creation. Until then: clone
the repo, `cargo run --release` in `engine/`, `npm run dev` in `ui/`,
`python -m copilot.service` in `copilot/`.)

## Stack

- **Engine**: Rust 1.88+ (`cpal`, `symphonia`, `rubato`, `midir`, `serde_json`)
- **Bridge**: Tauri 2 + WebSocket (tokio-tungstenite)
- **UI**: TypeScript + Vite + Web Audio API (visualization only)
- **Co-pilot**: Python 3.11 (vendors v1 analyzer + mashup primitives)

## Acknowledgements

This is the v2 of the [HypeHouse mixtape compiler](https://github.com/sandy9214/HypeHouse).
v1 (batch mixtape render) remains live for users who want offline render only.

~80 PRs were merged into v0.1.0 across the engine, copilot, UI, and infra over
the ~2-week active development window. Full changelog: [CHANGELOG.md](../CHANGELOG.md).

🤖 Built with help from a 5-LLM-family council (Claude, Codex, Gemini, Groq, Cohere).
