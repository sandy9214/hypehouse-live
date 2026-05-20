# Changelog

All notable changes to **hypehouse-live** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — Co-pilot (Python)
- **Cloud library sync (closes #102)** — Supabase-backed last-write-
  wins replication of the `tracks` table. Six-slice rollout:
  - `SyncClient` Protocol + `InMemorySyncClient` test fake (#148).
  - `SupabaseSyncClient` PostgREST adapter (stdlib urllib, no new
    dependency) + `migrations/001_tracks.sql` schema (#149).
  - Env-driven `build_sync_client_from_env` + bootstrap pull on
    copilot startup; falls back to InMemory when creds absent (#150).
  - `TrackLibrary` schema v10: `updated_at_micros` column + index;
    new `local_updated_at_micros` + `upsert_from_remote` methods.
    Real bootstrap-pull library merge (#151).
  - Schema v11: `pending_push` table; `add_track` enqueues outbound
    upsert; `LibrarySyncer.push_pending` + `bootstrap_push` drain
    the queue (#152).
  - `SyncDaemon` background thread runs pull + push every
    `HYPEHOUSE_SYNC_TICK_SECONDS` (default 60). Survives transient
    cloud errors; idempotent start/stop. (#153)
  - Operator wiring: set `SUPABASE_URL` + `SUPABASE_ANON_KEY` in
    copilot env; run `copilot/cloud_sync/migrations/001_tracks.sql`
    in the Supabase SQL editor; restart copilot.
- **`library.sync_status` RPC + daemon stats fold-in** — folds
  `last_pull_micros` / `last_push_micros` / `last_pull_fetched` /
  `last_pull_applied` / `last_push_pushed` / `last_tick_error` from
  the daemon into the JSON-RPC surface so the UI can render
  freshness + activity without polling the cloud (#155, #157).
- **`library.sync_now` RPC** — operator-driven force tick. Calls
  `SyncDaemon.tick_once()` out-of-band and returns the post-tick
  status payload. Missing-daemon → `-32000`; SyncError /
  `sqlite3.Error` → `-32603` with message (#161).
- **`library.list_pending_push` RPC** — returns `{"ids": [...]}` of
  tracks awaiting cloud push; UI uses this for the per-row chip
  (#165).
- **Exponential backoff on consecutive sync failures** — daemon
  doubles its sleep on each transport / DB / unexpected error,
  capped at `MAX_BACKOFF_SECONDS` (10 min). Resets on first clean
  tick. Lock-protected counter — safe under `sync_now` from the RPC
  thread + daemon loop from its own thread (#169).
- **`SyncStats.next_sync_micros` + UI countdown** — daemon stamps the
  next scheduled wake instant inside `_loop` right before
  `_stop.wait`. RPC folds it through; AboutPanel renders
  `· next in Xs` so operators know when the next auto-tick lands
  (varies with backoff). Field owned solely by `_loop` —
  out-of-band callers don't touch it (#174).
- **`SyncDaemon.wake_now()` + `library.sync_now` integration** —
  separate `_wake` event lets the RPC handler kick the daemon
  thread out of a long backoff wait after an out-of-band tick, so
  the next automatic tick fires at the reset cadence. `stop()` sets
  both events; older daemon stubs lacking `wake_now` tolerated via
  `getattr` (#176).
- **`library.requeue_all_pending` RPC** — operator escape hatch for
  pre-cloud-sync libraries. Single `INSERT OR IGNORE` against
  `pending_push` from `tracks`; returns the new queued count.
  Calls `wake_now` so the freshly filled queue starts draining
  immediately (#179).

### Added — UI (TypeScript)
- **AboutPanel "Library" row** — shows `N tracks · M pending sync`
  via the new `useSyncStatus(client)` hook (#155).
- **AboutPanel "Last sync" row** — relative-time string
  (`Xs/m/h/d ago`) backed by `last_pull_micros`. Tick-error suffix
  when the daemon reports a fault. `formatRelativeMicros` helper
  pure-tested across all bands incl. clock-skew (#159).
- **AboutPanel "sync now" button** — fires the new `library.sync_now`
  RPC and refreshes status; disabled while in-flight; surfaces RPC
  errors inline (#161).
- **AboutPanel tick-counts line** — when any of fetched / applied /
  pushed > 0, renders `↓ N fetched · M applied · ↑ K pushed`
  briefly so the operator can see what just synced (#163).
- **Per-row pending-sync chip** — TrackRow renders a small
  `⟳ pending` chip next to titles whose IDs are in the pending-push
  set. Library.tsx joins via the new `usePendingPushIds` hook
  (#167).
- **AboutPanel "queue all" button** — fires
  `library.requeue_all_pending` and pops an auto-dismissing
  "N queued for sync" toast. Errors land in the shared
  `about-sync-error` region. Auto-dismiss after 4s (#181).

### Changed
- `SyncDaemon` exception handling narrowed: `SyncError` /
  `sqlite3.Error` at WARN, everything else still escalated to ERROR
  with `exc_info` (#156).
- `SyncDaemon.wake_now(*, skip_next_tick=True)` — `library.sync_now`
  signals the daemon to skip its next automatic `tick_once` (the
  RPC already ran one out-of-band), avoiding the duplicate pull +
  push Codex flagged on #176. `library.requeue_all_pending` passes
  `skip_next_tick=False` so its freshly filled queue gets drained
  on the daemon's next iteration. Flag overwrites on every call so
  the latest caller's intent wins (#184).

### Docs
- New **docs/cloud-sync.md** — operator setup, verification surface,
  RLS / anon-key caveats (#171).
- New `make supabase-print` + `scripts/print_supabase_migrations.py`
  helper for operators without the supabase CLI (#171).
- New **docs/known-limitations.md** — v0.x caveats reference,
  replaces GH issue #93 as the source of truth. Covers Audio /
  Engine / Bridge / Co-pilot / Cloud sync / UI / Telemetry. README
  link updated; release notes link updated (#177).

## [0.1.0] — 2026-05-19

### Added — Engine (Rust)
- **`engine.session_info` WS RPC** — read-only snapshot of version +
  active output-device substring + 7 feature flags (MIDI clock IN/OUT,
  Ableton Link, Sentry, recording, rate-limit, shared CI). Pure handler
  reads env each call so mid-session flag flips are reflected on the
  next request (#144).
- **Sidechain gain-reduction atomic + bridge wire** — audio thread
  writes the live GR (dB × scale) to a shared `Arc<AtomicI16>`; bridge
  stamps it on every `engine.state_changed` payload as
  `sidechain_gain_reduction_db` so the UI can render a ducking meter
  without polling. End-of-render-block update cadence (~5 ms lag at
  256-frame chunks @ 48 kHz). Mirrors the master-limiter GR pattern
  (#141 / #142).
- **Sidechain compressor** — engine schema + DSP module + audio-path
  integration. Reducer-clamped params (threshold / ratio / attack /
  release / makeup), envelope follower + hard-knee ducker in the
  mixer's pre-crossfade path. Realtime-safe per ADR-004 — coefficients
  computed once per render block (#135 / #137 / #138).
- **Beat-FX one-shot** — `EffectOneShot { deck, slot, beats }` event +
  `OneShotState` (was_enabled, ends_at_micros, beat_period_ms_at_dispatch).
  Auto-disengage sweeper daemon polls snapshot at ~50 Hz and emits
  synthetic `EffectEnable` events tagged `EventSource::Internal`.
  Frozen beat_period_ms makes the schedule robust to mid-flight grid
  retunes (#127 / #131 / #132).
- **Output device selection** — `HYPEHOUSE_OUTPUT_DEVICE` env var routes
  master mix into a virtual sink (BlackHole / VB-Cable /
  pipewire-loopback) for software-only OBS/Twitch livestream capture
  without screen-share loopback. `engine.list_output_devices` WS RPC
  enumerates available devices for the UI picker (#113 / #115).
- `EventSource::Internal` variant — distinguishes control-thread daemon
  events from user / copilot inputs (#132).
- `EngineHandle::stamp_event(kind, source) -> Event` helper for
  control-thread daemons that inject synthetic events (#132).

### Added — UI (TypeScript)
- **AboutPanel** — consumer of the new `engine.session_info` RPC.
  Renders engine version + active audio sink + 7 feature flag chips
  (green=on, grey=off). One-shot fetch on mount; not subscribed to
  `state_changed` because the payload is session-static (#145).
- **Sidechain GR meter** — vertical bar in `SidechainPanel`, clamped
  0..-24 dB with amber→deep-orange gradient + 0/-12/-24 scale labels.
  Driven by the new `sidechain_gain_reduction_db` envelope field on
  every `engine.state_changed` notification (#143).
- **Sidechain compressor settings panel** — toggle + trigger-deck switch
  + 5 param knobs (threshold / ratio / attack / release / makeup), wires
  to the new engine events (#136).
- **Hot-cue markers on the waveform** — 8 color-coded Rekordbox-palette
  buttons overlaid on the scrolling waveform. Tap → jump, right-click →
  clear, drag (>4 px) → re-set position. ESC cancels in-flight drag
  (#122 / #134).
- **Output device picker** — settings dropdown listing engine-reported
  cpal devices; persists substring to localStorage. Shows
  "Restart engine to apply" hint when selection differs from active
  device (#121).
- **Beat-FX one-shot trigger row** — 1 / 4 / 8 / 16 beat preset buttons
  inside every assigned effect slot. Click emits `EffectOneShot`; live
  countdown ticks ms-remaining from `OneShotState.ends_at_micros`
  (#129).
- Anchored `performance.now()` countdown clock — monotonic sub-ms tick
  in the one-shot countdown, re-anchored every 500 ms against
  `Date.now()` so NTP corrections eventually surface without per-frame
  drift (#133).

### Changed
- `WAVEFORM_DEFAULT_WIDTH` / `WAVEFORM_DEFAULT_HEIGHT` exported from
  `Waveform.tsx` so overlay components (HotCueMarkers) share geometry
  constants without duplicated magic numbers (#126).
- `HotCueMarkers` — `activeSlots` memoised so the scroll-mode rAF skips
  6-7 of 8 iterations when most slots empty. Marker border color
  extracted to exported `HOTCUE_MARKER_BORDER` for future design-token
  swap (#139).
- `EffectEnable` / `EffectAssign` reducer arms clear any in-flight
  `one_shot` on the affected slot — explicit user toggle supersedes
  scheduled disengage (#127).

### Tests / CI
- Env-gated Windows shared-CI test ignores (closes #110): catch_unwind +
  sidechannel tests skip when `HYPEHOUSE_SHARED_CI_RUNNER=1` is set by
  the engine-ci workflow. Local Windows dev + self-hosted runners still
  execute (#116).
- Drain-via-loop fix for three sibling FP-accumulation flakes in
  `bridge::ratelimit::tests` — `tokens -= 1.0` × BURST_CAPACITY can
  leave the bucket slightly above 0 on some FP runtimes (#116 / #127 /
  #138).
- Cooperative env-override skip in `burst_allows_capacity_then_denies`
  to absorb parallel-test races on `RATE_LIMIT_DISABLED_ENV` (#138).
- Extract Vitest 4 + jsdom 29 localStorage polyfill to a shared
  `ui/src/test-utils/localStoragePolyfill.ts` helper — removes
  duplication across 4 test files (#125).
- Widened Win + macOS shared-CI-runner test tolerances for two flaky
  catch_unwind tests + const-time bearer-compare timing smoke check
  (#109).

## Initial v0.1.0 snapshot — 2026-05-19 (pre-consolidation draft from #114)

> The headline section above (`## [0.1.0]`) is the authoritative
> changelog entry for the v0.1.0 tag — the consolidated post-#114 work.
> The block below is kept verbatim from the original release-notes
> draft for historical reference; consolidated entries are now in the
> primary `[0.1.0]` section above.

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
