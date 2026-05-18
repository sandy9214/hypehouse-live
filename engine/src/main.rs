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
    EngineClock, SharedClock, SymphoniaDecodeService,
};
use hypehouse_engine::bridge::{self, EngineHandle};
use hypehouse_engine::persistence::{new_session_id, EventLog};
use hypehouse_engine::state::{EngineState, Event, EventKind};
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

    // MIDI clock OUT (ADR-007 §v0.1). Gated by both the `midi-clock-out`
    // Cargo feature AND the `MIDI_CLOCK_OUT_DEVICE` env var (substring
    // match against output port names; empty/unset = disabled).
    // Owned by `main` so the worker thread joins cleanly on shutdown.
    let _midi_clock_out = spawn_midi_clock_out_if_enabled(clock.shared.clone());

    // Event channel — fed by WS bridge / MIDI / co-pilot, drained by
    // the control-thread loop.
    let (event_tx, event_rx) = channel::unbounded::<Event>();

    // Persistent event log (ADR-003). One session id per process boot;
    // the log file lives under XDG_DATA_HOME (or the
    // HYPEHOUSE_EVENT_LOG_DIR override). Disabling is supported via
    // HYPEHOUSE_EVENT_LOG_DISABLED=1 — useful for ephemeral runs and
    // the CI matrix where the filesystem isn't durable.
    let session_id = new_session_id();
    let event_log = match EventLog::new(&session_id) {
        Ok(log) => {
            info!(
                session_id = %session_id,
                path = ?log.path(),
                disabled = log.is_disabled(),
                "event log opened"
            );
            Some(log)
        }
        Err(e) => {
            // Persistence failure is non-fatal — the live engine
            // continues without an audit trail. We warn loudly so
            // operators notice and fix the underlying cause (perms,
            // disk full).
            warn!(
                error = %e,
                session_id = %session_id,
                "event log open failed — continuing without persistence"
            );
            None
        }
    };

    // Control-thread loop runs on a dedicated OS thread so it doesn't
    // block the async runtime.
    let sample_rate = stream.sample_rate;
    let decode_for_control = Arc::clone(&decode_service);
    let shared_clock_for_control = clock.shared.clone();
    std::thread::spawn(move || {
        control_loop(
            event_rx,
            producer,
            clock,
            sample_rate,
            decode_for_control,
            shared_clock_for_control,
            event_log,
        )
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
///
/// Also appends every received event to the persistent log (ADR-003)
/// before emitting commands. Append happens **after** the reducer
/// applies — if the apply panicked we'd skip persistence, which is
/// fine: a panic is a bug, not a state change. Persistence errors are
/// downgraded to a warn so a transient disk hiccup never kills the
/// live set.
fn control_loop(
    event_rx: Receiver<Event>,
    mut producer: AudioProducer,
    clock: EngineClock,
    sample_rate: u32,
    decode: Arc<dyn DecodeService>,
    shared_clock: SharedClock,
    mut event_log: Option<EventLog>,
) {
    let mut state = EngineState::default();

    while let Ok(ev) = event_rx.recv() {
        let next = state.apply(&ev);

        // Side-channel: propagate master_bpm to the SharedClock so the
        // MIDI clock OUT tick thread (ADR-007 §v0.1) can re-derive its
        // period without polling EngineState. The reducer has already
        // validated the value.
        if let EventKind::SetMasterBpm { bpm } = &ev.kind {
            shared_clock.set_master_bpm(*bpm);
        }

        // Persist BEFORE emitting commands so a downstream panic
        // (e.g. translator bug) can be reproduced from the log.
        if let Some(log) = event_log.as_mut() {
            if let Err(e) = log.append(&ev) {
                warn!(error = %e, event_id = ev.id, "event log: append failed");
            }
        }

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

    // Flush tail on graceful shutdown (channel closed = bridge gone).
    // Drop would do this too, but doing it explicitly surfaces errors
    // in the log rather than swallowing them.
    if let Some(mut log) = event_log.take() {
        if let Err(e) = log.flush() {
            warn!(error = %e, "event log: shutdown flush failed");
        }
    }
    info!("control loop: event channel closed — shutting down");
}

/// Spawn the MIDI clock OUT worker if the user has configured it.
///
/// Selection rules (per ADR-007 §"Open questions"):
/// * Env var `MIDI_CLOCK_OUT_DEVICE` unset / empty → disabled, returns `None`.
/// * `midi-clock-out` feature off at compile time → disabled even if env set.
/// * Substring match (case-insensitive) against the available output port
///   names. No match → log a warning, return `None`. We never fail the
///   whole engine on a missing MIDI device — DJ rigs commonly boot
///   without all the hardware plugged in.
fn spawn_midi_clock_out_if_enabled(
    shared_clock: SharedClock,
) -> Option<hypehouse_engine::midi::MidiClockOut> {
    let device = std::env::var("MIDI_CLOCK_OUT_DEVICE").unwrap_or_default();
    let _ = shared_clock; // keep signature stable when feature is off
    if device.trim().is_empty() {
        info!("midi-clock-out: MIDI_CLOCK_OUT_DEVICE unset — disabled");
        return None;
    }

    #[cfg(feature = "midi-clock-out")]
    {
        match hypehouse_engine::midi::MidiClockOut::start(Some(&device), shared_clock) {
            Ok(handle) => {
                info!(
                    port = %handle.port_name,
                    device_filter = %device,
                    "midi-clock-out: started — emitting 24 PPQN @ master_bpm"
                );
                Some(handle)
            }
            Err(e) => {
                warn!(
                    error = %e,
                    device_filter = %device,
                    "midi-clock-out: failed to open output port — continuing without"
                );
                None
            }
        }
    }
    #[cfg(not(feature = "midi-clock-out"))]
    {
        warn!(
            device_filter = %device,
            "midi-clock-out: env var set but feature `midi-clock-out` not enabled at compile time"
        );
        None
    }
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
