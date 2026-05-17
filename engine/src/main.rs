//! hypehouse-engine — entry point.
//!
//! Boots:
//!   1. Audio device (cpal default output, ADR-004).
//!   2. Control-thread loop that pulls `Event`s from the event channel,
//!      folds them into `EngineState`, and emits `AudioCommand`s onto
//!      the SPSC ring.
//!   3. WebSocket bridge to the UI + copilot (tokio-tungstenite).
//!   4. MIDI input listener (midir) — wired in a later PR.
//!
//! Real work lives in `lib.rs` so we can unit-test it without spinning up cpal.
//!
//! The WS bridge is wired here. The audio thread + MIDI listener share
//! the same `EngineHandle` so events from any source fan out as
//! `engine.state_changed` notifications to every connected client.

use anyhow::Result;
use crossbeam::channel::{self, Receiver, Sender};
use hypehouse_engine::audio::{
    event_to_commands, io::spawn_audio_thread, AudioProducer, AudioRing, EngineClock,
    StubDecodeService,
};
use hypehouse_engine::bridge::{self, EngineHandle};
use hypehouse_engine::state::{EngineState, Event};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "hypehouse-engine starting"
    );

    // SPSC ring: producer → control thread, consumer → audio thread.
    let (producer, consumer) = AudioRing::new().split();

    // Engine clock — the audio thread bumps it; we read it from the
    // control thread for command scheduling.
    let clock = EngineClock::new(48_000, 120.0);

    // Spawn the audio thread (cpal stream). Holds the stream alive
    // for the duration of `main`.
    let stream = spawn_audio_thread(consumer, clock.shared.clone())?;
    info!(
        sample_rate = stream.sample_rate,
        channels = stream.channels,
        "audio thread up — cpal stream playing"
    );

    // Event channel — fed by WS bridge / MIDI / co-pilot, drained by
    // the control-thread loop.
    let (event_tx, event_rx) = channel::unbounded::<Event>();

    // Control-thread loop runs on a dedicated OS thread so it doesn't
    // block the async runtime.
    let sample_rate = stream.sample_rate;
    std::thread::spawn(move || control_loop(event_rx, producer, clock, sample_rate));

    let engine = EngineHandle::new();
    let config = bridge::BridgeConfig::from_env();
    let server = bridge::spawn(config, engine).await?;
    info!(addr = %server.local_addr, "ws bridge ready");

    // Keep event_tx alive so the control loop doesn't exit while the
    // bridge is still running. Future PRs will wire bridge → event_tx
    // so UI/copilot RPC calls flow into the engine.
    let _event_tx: Sender<Event> = event_tx;

    // Graceful shutdown on SIGINT (Ctrl-C) or SIGTERM. The accept loop
    // selects on the cancel oneshot, drains in-flight client tasks, and
    // returns; we then await the server task and exit zero.
    shutdown_signal().await;
    info!("shutdown signal received — closing bridge");
    server.shutdown().await?;

    Ok(())
}

/// Control-thread loop: pull events, fold state, emit audio commands.
/// Blocks on the event channel; exits when the channel is closed.
fn control_loop(
    event_rx: Receiver<Event>,
    mut producer: AudioProducer,
    clock: EngineClock,
    sample_rate: u32,
) {
    let mut state = EngineState::default();
    let mut decode = StubDecodeService::new();

    while let Ok(ev) = event_rx.recv() {
        let next = state.apply(&ev);
        let now_frame = clock.frame();
        let cmds = event_to_commands(&state, &next, &ev, now_frame, sample_rate, &mut decode);
        for cmd in cmds.into_iter() {
            if let Err(dropped) = producer.try_push(cmd) {
                warn!(
                    ?dropped,
                    "audio ring full — dropping command (control plane backpressure)"
                );
            }
        }
        state = next;
    }
    info!("control loop: event channel closed — shutting down");
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => info!("got SIGTERM"),
        _ = sigint.recv() => info!("got SIGINT"),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.ok();
}
