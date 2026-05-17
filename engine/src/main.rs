//! hypehouse-engine — entry point.
//!
//! Boots:
//!   1. Audio device (cpal default output, ADR-004).
//!   2. Control-thread loop that pulls `Event`s from a placeholder
//!      channel feed, folds them into `EngineState`, and emits
//!      `AudioCommand`s onto the SPSC ring.
//!   3. MIDI input listener (midir) — separate PR.
//!   4. WebSocket bridge to the UI (tokio-tungstenite) — separate PR.
//!
//! Real work lives in `lib.rs` so we can unit-test it without spinning up cpal.

use anyhow::Result;
use crossbeam::channel::{self, Receiver};
use hypehouse_engine::audio::{
    event_to_commands, io::spawn_audio_thread, AudioProducer, AudioRing, EngineClock,
    StubDecodeService,
};
use hypehouse_engine::state::{EngineState, Event};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
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

    // Placeholder event source. Real MIDI / WS / co-pilot lands in
    // future PRs; for this PR we accept events via a crossbeam channel
    // so smoke-tests can drive the engine programmatically.
    let (_event_tx, event_rx) = channel::unbounded::<Event>();

    control_loop(event_rx, producer, clock, stream.sample_rate);

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
