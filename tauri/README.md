# hypehouse-desktop — Tauri shell

Packages the Rust audio engine (`engine/`) and the TypeScript UI (`ui/`)
into a single desktop binary. Implements ADR-001 (Rust + Tauri + WebMIDI).

## What lives here

| File | Purpose |
|---|---|
| `Cargo.toml` | Tauri v2 + sidecar plumbing + rand for the session token. |
| `tauri.conf.json` | App id `live.hypehouse.desktop`, 1400×900 window, builds the UI from `../ui/dist`. |
| `build.rs` | Stock `tauri_build::build()` — generates platform glue. |
| `src/main.rs` | Runtime entrypoint. Spawns the engine sidecar then runs Tauri. |
| `src/lib.rs` | Re-exports `commands` and `sidecar` so integration tests can import them. |
| `src/commands.rs` | `get_bridge_url` + `get_bridge_token` invoke handlers. |
| `src/sidecar.rs` | `ChildGuard` — spawns + reaps the engine binary. |
| `tests/sidecar_spawn.rs` | Integration test — sleep-shim child gets killed on Drop. |

## Quickstart

### Development

```bash
# One-time: install the Tauri CLI matching the v2 crate.
cargo install tauri-cli@2

# Build the engine in release mode (Tauri spawns this binary at runtime).
cargo build --release -p hypehouse-engine --manifest-path ../engine/Cargo.toml

# Launch the desktop shell. Vite dev server + Rust window come up together.
cd tauri && cargo tauri dev
```

### Production build

```bash
# Builds the UI (npm run build inside ../ui), then bundles the Tauri app.
cd tauri && cargo tauri build
```

Output goes to `tauri/target/release/bundle/`. For v0.1 we ship the
Tauri defaults (`.app` on macOS, `.exe` on Windows, AppImage stub on
Linux). DMG/MSI/AppImage polish + code signing land in a follow-up
release-prep PR (see ADR-001 §"Open questions").

## How the UI finds the engine

1. Tauri main generates a 32-byte hex bearer token on launch.
2. Token is injected into the engine sidecar via the
   `HYPEHOUSE_BRIDGE_TOKEN` env var **before** the child process starts.
3. UI calls `invoke('get_bridge_url')` → `ws://127.0.0.1:8765` (or the
   `HYPEHOUSE_BRIDGE_URL` env override).
4. UI calls `invoke('get_bridge_token')` → same hex token.
5. UI's `JsonRpcWS` sends `auth.hello` with that token as its first
   frame; engine accepts the bearer and the regular RPC traffic flows.

When the UI runs in plain-browser mode (no Tauri), `ui/src/runtime.ts`
falls back to `VITE_BRIDGE_URL` / `dev-token` so day-to-day Vite dev
keeps working unchanged.

## Out of scope for this PR

* Code signing (Apple notarisation / Authenticode / Linux package
  signing) — release-prep PR.
* Auto-updater — pending an ADR on update channel + signing key
  custody.
* Bespoke installer images (DMG with custom layout, MSI with WiX
  template, polished AppImage) — Tauri defaults are enough to validate
  the desktop architecture.

## Why a separate process for the engine?

* `cpal` opens a real-time audio thread. Co-tenanting that with the
  WebKit-backed Tauri runtime risks priority inversion on the audio
  callback when the renderer's GPU thread spikes.
* Crash isolation: a panic in the audio thread shouldn't take down
  the UI mid-set.
* Keeps the engine binary usable headless (browser-only dev, CI smoke
  tests) without dragging Tauri's ~150 MB of dependencies into every
  build.
