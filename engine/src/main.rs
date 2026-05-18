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
use crossbeam::channel::{self, Receiver};
use hypehouse_engine::audio::{
    event_to_commands, io::spawn_audio_thread, AudioProducer, AudioRing, DecodeService,
    EngineClock, SymphoniaDecodeService,
};
use hypehouse_engine::bridge::{self, EngineHandle};
use hypehouse_engine::state::{EngineState, Event};
use std::sync::Arc;
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

    // Streaming decode service. One service, two clones:
    //   - audio thread (via the mixer) calls `read` to pull frames.
    //   - control thread (via the translator) calls `open`/`close`.
    // Both are alloc-free where it matters; see `decode.rs` module
    // docs.
    let decode_service: Arc<dyn DecodeService> = Arc::new(SymphoniaDecodeService::new());

    // Spawn the audio thread (cpal stream). Holds the stream alive
    // for the duration of `main`. The mixer carries an Arc clone of
    // the decode service so cpal's callback can pull stereo frames.
    let stream = spawn_audio_thread(consumer, clock.shared.clone(), Arc::clone(&decode_service))?;
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
    let decode_for_control = Arc::clone(&decode_service);
    std::thread::spawn(move || {
        control_loop(event_rx, producer, clock, sample_rate, decode_for_control)
    });

    // Bridge handle is wired to the control-loop event channel so every
    // accepted `engine.submit_event` RPC flows into `event_rx`. Cloning
    // `event_tx` keeps a sender alive inside the handle for the bridge's
    // lifetime — the control loop will only see `recv` return Err once
    // both the bridge handle (and its clones) and the local `event_tx`
    // are dropped during shutdown.
    let engine = EngineHandle::with_event_sink(event_tx.clone());
    let config = bridge::BridgeConfig::from_env();
    let server = bridge::spawn(config, engine).await?;
    info!(addr = %server.local_addr, "ws bridge ready");

    // Drop the local sender now that the bridge owns its own clone. The
    // control loop continues to receive events from any handle clone
    // (e.g. additional sources wired in later PRs).
    drop(event_tx);

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
    decode: Arc<dyn DecodeService>,
) {
    let mut state = EngineState::default();

    while let Ok(ev) = event_rx.recv() {
        let next = state.apply(&ev);
        let now_frame = clock.frame();
        let cmds = event_to_commands(&state, &next, &ev, now_frame, sample_rate, decode.as_ref());
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
