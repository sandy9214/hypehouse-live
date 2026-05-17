//! hypehouse-engine — entry point.
//!
//! Boots:
//!   1. Audio device (cpal default output).
//!   2. MIDI input listener (midir).
//!   3. WebSocket bridge to the UI (tokio-tungstenite).
//!   4. Event log + reducer loop.
//!
//! Real work lives in `lib.rs` so we can unit-test it without spinning up cpal.

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .json()
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "hypehouse-engine starting"
    );

    // v0.1 skeleton — wires up boot but does no audio yet. Subsequent PRs
    // will fill in the audio thread + MIDI listener + WS bridge.
    info!("engine boot — placeholder; audio + MIDI + WS land in v0.1.1");

    Ok(())
}
