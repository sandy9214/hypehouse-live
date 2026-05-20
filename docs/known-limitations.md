# Known limitations + caveats (v0.x)

Pinned reference for known constraints. Update as features mature.
Source-of-truth doc — replaces the now-closed GitHub issue #93.

## Audio

- Stem mode caches **~125 MB/track** (4× stereo 16-bit WAV at 44.1k).
  200-track library ≈ **25 GB**.
- Demucs first run downloads ~2 GB model + CPU stem extraction takes
  ~3 min per track (GPU ~30 s).
- WSOLA pitch+tempo at extreme ratios (±50%+) introduces audible
  artifacts. Single-knob movements stay on the cheaper SRC path.

## Engine

- Event-log retention defaults: keep last 50 sessions OR younger than
  30 days, whichever yields more. Configure via
  `HYPEHOUSE_LOG_MAX_DAYS` / `HYPEHOUSE_LOG_MIN_KEEP`.
- Recording auto-disable: set `HYPEHOUSE_RECORDING_DISABLED=1`.
- MIDI clock OUT disables when IN is active (single source-of-truth,
  no feedback loops). Tracked in ADR-007.
- **Ableton Link v0.2 shipped as a stub only** — real `rust-link`
  integration deferred pending LGPL sign-off (ADR-009).
- **Sidechain compressor: schema + UI only, DSP deferred.** The
  `SetSidechainEnabled` event and the sidechain settings panel
  persist + render, but the audio path does NOT yet duck the
  non-trigger deck. See `docs/api/ws-protocol.md` (the
  `SetSidechainEnabled` row notes "currently schema-only") and
  `ui/src/components/SidechainPanel.tsx`. Tracked as a DSP-wiring
  follow-up PR.
- **Output device selection is restart-only.** Picking a different
  sink from the dropdown persists the substring but does not
  hot-swap the live `cpal` stream — the UI shows "Restart engine to
  apply." Tearing down + rebuilding a `cpal` Stream mid-render is
  deferred (ADR-TBD).

## Bridge

- Engine bridge runs on `127.0.0.1:8765`. Auth via bearer token at the
  HTTP-upgrade, or in-band `auth.hello` JSON-RPC.
- 5 s pending-auth timeout, then 1008 close.

## Co-pilot

- aiohttp JSON-RPC on `127.0.0.1:8766/rpc`. Engine bridge proxies
  `library.*` calls to this.
- LUFS analysis happens lazy on `library.add_track_from_path` — if
  `pyloudnorm` isn't installed, tracks ingest without gain
  compensation.

## Cloud library sync

See **[docs/cloud-sync.md](./cloud-sync.md)** for the full operator
guide. Key v0.x caveats:

- RLS is OFF by default — fine for single-user; flip on in the
  migration SQL for multi-user.
- Conflict resolution = last-write-wins on `updated_at_micros`. No
  merge. Two-machine edits inside the same ~60s window can clobber.
- Daemon backs off exponentially on consecutive errors, capped at
  10 minutes. The AboutPanel "next in Xs" countdown reflects the
  real schedule.
- Pre-v10 local rows aren't auto-enqueued for first push. Re-add or
  use the (future) `library.requeue_all_pending` RPC to seed the
  cloud from an existing local library.

## UI

- Tauri shell signs binaries via the release workflow **only** when
  `TAURI_UPDATER_PRIVATE_KEY` + per-OS certs are present in GH
  secrets.
- Mobile responsive (<768 px) shows the single-deck swiper. Library
  is a bottom drawer.
- Static demo (web-only mode) at
  <https://sandy9214.github.io/hypehouse-live/> once the Pages
  workflow lands.

## Telemetry

- Opt-in only. `HYPEHOUSE_TELEMETRY_ENABLED=1` env or
  `~/.config/hypehouse-live/telemetry.toml`.
- DSN placeholder is bogus by default — operator overrides via
  `HYPEHOUSE_TELEMETRY_DSN`.
