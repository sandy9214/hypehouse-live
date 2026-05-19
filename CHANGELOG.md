# Changelog

All notable changes to **hypehouse-live** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Output device selection via `HYPEHOUSE_OUTPUT_DEVICE` env var — software-only
  livestream support via virtual loopback devices (BlackHole / VB-Cable /
  pipewire-loopback). Engine routes master mix into a virtual sink so
  OBS / Twitch can capture lossless audio without screen-share loopback (#113).

### Tests
- Widened Windows + macOS shared-CI-runner test tolerances for two flaky
  catch_unwind tests + the const-time bearer-compare timing smoke check (#109).

## [0.1.0] — 2026-05-19

Initial public preview. Software-only AI-augmented DJ tool. Closest peer:
[Mixxx](https://mixxx.org). Differentiator: AI co-pilot with mashability-scored
auto-mix + stem-aware mixing + LUFS-normalized library + cloud-shareable preset
snapshots + crowd-pleaser export.

**Hardware out of scope** (2026-05-18 pivot): no Pioneer CDJ/DDJ-specific timing,
no ProDJ Link, no vinyl-mode scratch hardware emulation. WebMIDI works as a
fallback when a controller IS present but is NOT the primary input — keyboard +
mouse + UI are.

### Added — Engine (Rust)
- 2-deck audio engine on `cpal` + `symphonia` + `rubato` + `midir`
  (ADR-001/002/004) (#1, #2, #3, #7).
- WebSocket + JSON-RPC bridge with bearer-token auth (browser + native modes)
  (#3, #9, #32).
- Streaming `symphonia` decode service replaces stub (#29).
- HTTP MediaSource — open remote streaming URLs directly (#108).
- Pitch/tempo-independent playback per-deck via rubato time-stretch (#43); WSOLA
  stage 2 for true pitch/tempo orthogonality (#63).
- Effects chain v0.1 — filter / echo / reverb / gate (ADR-006) (#31).
- Gate effect subscribes to live master BPM via SharedClock (#49).
- Master-bus soft-clip limiter with gain-reduction telemetry (#52, #54).
- Crossfader curve options — linear / dipped / sharp / scratch (#72).
- Event-sourced state log + replay (ADR-003) (#42, #51, #73).
- Master-mix recording to WAV per session (#44, #71).
- Crowd-pleaser export — auto-trim setlist + chapter markers (#100).
- MIDI clock OUT v0.1 — engine acts as master, 24 PPQN to hardware
  sequencers (ADR-007 §v0.1) (#35).
- MIDI clock IN v0.3 — engine acts as slave, locks `master_bpm` to external
  master with 4-beat smoothing + ±0.1 BPM deadband (ADR-007 §v0.3) (#62, #70).
- Ableton Link scaffold + ADR-007 §v0.2 design (full impl deferred) (#64).
- Stem-aware deck playback (4 channels per deck) (#76, #77).
- Loop bar presets — 1/2/4/8/16 bars, beat-grid aware (#98).
- Rate-limit `submit_event` per client — 200/sec token bucket, 1000 burst,
  `-32003 RATE_LIMITED` error code (#99).
- Performance dashboard — CPU + audio underrun stats (#80).
- Decoder panic surfacing via `catch_unwind` + sidechannel (#56, #75).
- Persistent event log + retention pruning (closes #41) (#73).
- Opt-in Sentry telemetry hook (#60).

### Added — Co-pilot (Python)
- Vendored v1 analyzer + JSON-RPC client + decision loop (#4).
- Beat-grid + downbeat detection on track load (#30).
- Library proxy via aiohttp JSON-RPC server (#53, #55).
- Real waveform rendering from decode peaks (#50).
- Hot cues persisted to library DB + restored on track reload (#46).
- Demucs stem separation scaffold — vocals/drums/bass/other (#57).
- Auto-mix mode — proposer executes transitions without user prompts (#67).
- Smart filters — BPM range + Camelot key compatibility (#74).
- LUFS target leveler — auto-normalize tracks to -14 LUFS (#83).
- Smart key transposer — auto-pitch incoming deck to match active deck (#84).
- Auto-DJ playlist queue with priority + reordering (#96).
- SoundCloud streaming source — search + add to library (#107).

### Added — UI (TypeScript)
- 2-deck scaffold + WebSocket JSON-RPC client (ADR-001) (#5).
- WebMIDI input + Pioneer DDJ-200 mapping + keyboard fallback (#6).
- Deck controls + crossfader + hot cues wired to `submit_event` (#28).
- Track library browse + drag-load (#40).
- Effects rack + `engine.list_effects` manifest (#38).
- Effects chain slot reordering via drag-drop (#66).
- Master limiter control + gain-reduction meter (#54).
- Replay UI — browse + load past session event logs (#51).
- Scrolling waveform with animated playhead (#65).
- Per-stem mute toggles + DeckLoadStems trigger (#77).
- Save/load user preset snapshots — effects + EQ + crossfader curve (#78).
- Cue countdown — next-downbeat + phrase indicator per deck (#79).
- Performance dashboard (#80).
- Mobile / tablet responsive layout (≥360px viewport) (#82).
- BPM lock indicator badge — clock IN active state (#70).
- WebMIDI mapping hot-reload + custom mapping import (#97).
- Per-track waveform hover preview in library (#95).
- First-launch onboarding wizard — library ingest (#58).
- Sessions snapshot pane + export button (#100).

### Added — Desktop
- Tauri shell wrapping UI + engine into single distributable (#39).
- Tauri code signing + auto-updater scaffold (ADR-008) (#59).

### Added — Infra
- 3-OS CI matrix (Linux + macOS + Windows) for the engine (#8, #22, #33).
- Rust 1.88 toolchain (#34).
- Dependabot for Cargo + npm + GitHub Actions (#8).
- CodeQL security scan (security-extended on push + PR + weekly) (#8).
- GitHub Pages deploy of UI static demo on main push (#94).
- CODEOWNERS — solo dev today, ready for team growth (#90).
- Auto PR review workflow via GitHub Models (#8).

### Added — ADRs
- ADR-001: Choose Rust + Tauri + WebMIDI.
- ADR-002: 2-deck + co-pilot primitive.
- ADR-003: Event-sourced state log.
- ADR-004: cpal audio-thread realtime contract.
- ADR-005: Reserved.
- ADR-006: Effects chain v0.1.
- ADR-007: MIDI clock + Ableton Link (v0.1 / v0.2 / v0.3).
- ADR-008: Tauri code signing + auto-updater.
- ADR-009: Peer clock backend split.

### Known limitations (see issue #93)
- Tauri binaries unsigned (signing decision deferred — paid Apple/Microsoft
  cert required).
- LFO modulation scaffold not yet wired into effects chain (WIP, #89).
- Ableton Link real backend not wired (v0.2.x deferred).
- Cloud library sync deferred to v0.2 (#102).
- Live-stream output (engine-level virtual loopback) at engine slice only;
  WS RPC + UI device picker deferred.

[Unreleased]: https://github.com/sandy9214/hypehouse-live/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sandy9214/hypehouse-live/releases/tag/v0.1.0
