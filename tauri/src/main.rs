//! hypehouse-desktop — Tauri v2 shell.
//!
//! Boot sequence (mirrors `engine/src/main.rs` for symmetry):
//!   1. tracing-subscriber up (env-filter so prod can `RUST_LOG=info`).
//!   2. Generate per-launch bearer token via `commands::session_token`.
//!   3. Spawn engine sidecar with `HYPEHOUSE_BRIDGE_TOKEN` injected;
//!      store the `ChildGuard` in Tauri's managed state so command
//!      threads can introspect it (e.g. for future restart endpoints).
//!   4. Build the Tauri app, register the two invoke commands, run.
//!
//! On app exit, the managed `ChildGuard` Drop fires and kills + reaps
//! the engine. We do NOT rely on the OS to clean up child processes —
//! macOS in particular happily leaves orphan processes alive when the
//! parent exits abnormally.
//!
//! Browser-only / dev mode: this binary is *optional*. UI also runs
//! standalone via `npm --prefix ui run dev` against a manually-started
//! engine — `tauri/src/runtime.ts` on the UI side detects which mode
//! it's in.

// Windows: suppress the spawned console window in release builds.
// Dev builds keep the console for log output convenience.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use hypehouse_desktop::sidecar::ChildGuard;
use hypehouse_desktop::{commands, sidecar};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn main() {
    // tracing first — every later log call relies on it.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "hypehouse-desktop starting"
    );

    // Generate the session token *before* spawning the engine — the
    // engine reads HYPEHOUSE_BRIDGE_TOKEN at boot and refuses
    // auth.hello calls that don't match. UI then fetches the same
    // token via the `get_bridge_token` invoke command.
    let token = commands::session_token().to_string();

    // Spawn engine sidecar. A spawn failure is fatal — the app is
    // useless without it. We surface the error via logs + nonzero
    // exit so CI / crash reporters can see it.
    let engine_bin = sidecar::resolve_engine_path();
    let guard = match ChildGuard::spawn(engine_bin.clone(), &token) {
        Ok(g) => g,
        Err(e) => {
            error!(
                error = %e,
                bin = %engine_bin.display(),
                "fatal: failed to spawn engine sidecar — exiting"
            );
            // We don't panic — that would print a noisy backtrace into
            // the user's console. Clean exit code is friendlier for
            // Tauri's bundled launchers.
            std::process::exit(1);
        }
    };

    // Run Tauri. `.manage(guard)` moves the ChildGuard into the
    // application state; its Drop fires when Tauri's runtime exits,
    // which happens after the last window closes.
    tauri::Builder::default()
        .manage(guard)
        .invoke_handler(tauri::generate_handler![
            commands::get_bridge_url,
            commands::get_bridge_token
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            error!(error = %e, "tauri runtime crashed");
            std::process::exit(1);
        });

    info!("hypehouse-desktop exiting cleanly");
}
