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
    event_to_commands_with_errors, io::spawn_audio_thread, AudioProducer, AudioRing, DecodeService,
    EngineClock, SharedClock, SymphoniaDecodeService,
};
use hypehouse_engine::bridge::{self, EngineHandle};
use hypehouse_engine::clock_sync::{LinkStub, PeerClock};
use hypehouse_engine::persistence::{new_session_id, resolve_log_root, retention, EventLog};
use hypehouse_engine::recording::MasterRecorder;
use hypehouse_engine::state::{EngineState, Event, EventKind};
use hypehouse_engine::telemetry;
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

    // Opt-in Sentry telemetry. Returns `None` (and contacts no
    // upstream) unless the operator has explicitly enabled it via env
    // var or config file. The guard MUST live for the whole of `main`
    // so its `Drop` impl flushes any in-flight events on shutdown —
    // hoisting it here means a panic anywhere below this line is
    // still captured.
    let _sentry_guard = telemetry::init_telemetry();

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

    // Persistent event log (ADR-003). One session id per process boot;
    // the log file lives under XDG_DATA_HOME (or the
    // HYPEHOUSE_EVENT_LOG_DIR override). Disabling is supported via
    // HYPEHOUSE_EVENT_LOG_DISABLED=1 — useful for ephemeral runs and
    // the CI matrix where the filesystem isn't durable.
    //
    // We create the session id BEFORE the audio stream so the same id
    // is used for both the event log and the master-mix recorder
    // (they share the on-disk session directory).
    let session_id = new_session_id();

    // Per-session master-mix recorder (master.wav under the session
    // directory). Honours `HYPEHOUSE_RECORDING_DISABLED=1` — when set,
    // `try_new_from_env` returns `Ok(None)` and the mixer runs without
    // a tee. Persistence failure is non-fatal: the live engine
    // continues without an audio recording.
    let recording_path = resolve_recording_path(&session_id);
    let (mut master_recorder, recorder_sink) = match MasterRecorder::try_new_from_env(
        &recording_path,
        // We don't yet know the device sample rate — use 48 kHz as the
        // working default. cpal will pick the device's preferred rate
        // below; we re-open the recorder with the real rate if it
        // differs. This avoids racing the file create against the
        // device probe.
        48_000,
    ) {
        Ok(Some((rec, sink))) => {
            info!(
                session_id = %session_id,
                path = %rec.path().display(),
                "master-mix recorder opened"
            );
            (Some(rec), Some(sink))
        }
        Ok(None) => {
            info!(
                session_id = %session_id,
                "master-mix recorder disabled by env"
            );
            (None, None)
        }
        Err(e) => {
            warn!(
                error = %e,
                session_id = %session_id,
                path = %recording_path.display(),
                "master-mix recorder open failed — continuing without recording"
            );
            (None, None)
        }
    };

    // Optional output-device override for the livestream / virtual-loopback
    // use case (issue #111). Setting `HYPEHOUSE_OUTPUT_DEVICE` to a fragment
    // of a cpal device name (e.g. `BlackHole`, `VB-Cable`, `pipewire-loopback`)
    // routes the engine's master mix into a virtual sink so OBS / Twitch can
    // capture lossless audio without screen-share loopback. Empty string =
    // ignore. Bad fragment = log warn + fall back to host default.
    let output_device_env = std::env::var("HYPEHOUSE_OUTPUT_DEVICE").ok();
    let output_device_arg = output_device_env
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(name) = output_device_arg {
        info!(substring = %name, "honouring HYPEHOUSE_OUTPUT_DEVICE override");
    }

    // Spawn the audio thread (cpal stream). Holds the stream alive
    // for the duration of `main`. The mixer carries an Arc clone of
    // the decode service so cpal's callback can pull stereo frames.
    let stream = spawn_audio_thread(
        consumer,
        clock.shared.clone(),
        Arc::clone(&decode_service),
        recorder_sink,
        output_device_arg,
    )?;
    info!(
        sample_rate = stream.sample_rate,
        channels = stream.channels,
        "audio thread up — cpal stream playing"
    );

    // Peer clock backend (ADR-007 §v0.2). Default = `LinkStub` (no-op
    // backend that returns 120 BPM / 0 peers). When the `ableton-link`
    // Cargo feature is enabled AND `ABLETON_LINK_ENABLED=1` is set in
    // the environment, we'd swap in the real backend — but the
    // v0.2.x follow-up PR (see ADR-007 §v0.2 + ADR-009) lands the
    // actual `rust-link` binding. Today the "real" path simply panics
    // with `unimplemented!()` on first use, so we keep it walled off
    // behind both the compile-time feature AND the runtime env flag.
    let peer_clock: Arc<dyn PeerClock> = build_peer_clock();
    info!(
        backend = peer_clock_backend_label(),
        tempo = peer_clock.current_tempo(),
        peers = peer_clock.peer_count(),
        "PeerClock backend wired (ADR-007 §v0.2 scaffold)"
    );
    // Currently we don't fan the peer-clock readings into the audio
    // thread — that's the v0.2.x wiring step. Keeping the handle alive
    // for the duration of `main` so the eventual UI bridge can read it.
    let _peer_clock = peer_clock;

    // MIDI clock IN (ADR-007 §v0.3). Gated by both the `midi-clock-in`
    // Cargo feature AND the `MIDI_CLOCK_IN_DEVICE` env var. When
    // active it locks `SharedClock::master_bpm` to an external master
    // sequencer / DAW. Spawn it BEFORE the OUT so the OUT can detect
    // the IN is active and silently disable itself (avoids feedback
    // loop — see clock_in.rs module docs).
    let _midi_clock_in = spawn_midi_clock_in_if_enabled(clock.shared.clone());
    let clock_in_active = _midi_clock_in.is_some();

    // MIDI clock OUT (ADR-007 §v0.1). Gated by both the `midi-clock-out`
    // Cargo feature AND the `MIDI_CLOCK_OUT_DEVICE` env var (substring
    // match against output port names; empty/unset = disabled). When
    // MIDI clock IN is active (v0.3) we silently skip OUT so the
    // engine can't echo its own input back to the master.
    // Owned by `main` so the worker thread joins cleanly on shutdown.
    let _midi_clock_out = if clock_in_active {
        info!(
            "midi-clock-out: skipped — MIDI clock IN is active (avoids feedback loop, ADR-007 §v0.3)"
        );
        None
    } else {
        spawn_midi_clock_out_if_enabled(clock.shared.clone())
    };

    // Event channel — fed by WS bridge / MIDI / co-pilot, drained by
    // the control-thread loop.
    let (event_tx, event_rx) = channel::unbounded::<Event>();

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

    // Event-log retention sweep (issue #41). Runs AFTER `EventLog::new`
    // so the current session directory already has a fresh mtime —
    // newest-first sort therefore keeps it out of the deletion
    // candidate set automatically. `prune_from_env` honours
    // `HYPEHOUSE_LOG_MAX_DAYS` / `HYPEHOUSE_LOG_MIN_KEEP` /
    // `HYPEHOUSE_LOG_RETENTION_DISABLED` and logs a single info line
    // with the summary; failures are non-fatal.
    match resolve_log_root() {
        Ok(root) => match retention::prune_from_env(&root) {
            Ok(summary) => info!(
                deleted = summary.deleted,
                retained = summary.retained,
                bytes_freed = summary.bytes_freed,
                "event log retention: pruned"
            ),
            Err(e) => warn!(error = %e, "event log retention: sweep failed — continuing"),
        },
        Err(e) => warn!(error = %e, "event log retention: skipped — root unresolved"),
    }

    // Bridge handle is wired to the control-loop event channel so every
    // accepted `engine.submit_event` RPC flows into `event_rx`. Cloning
    // `event_tx` keeps a sender alive inside the handle for the bridge's
    // lifetime — the control loop will only see `recv` return Err once
    // both the bridge handle (and its clones) and the local `event_tx`
    // are dropped during shutdown.
    //
    // The handle is also cloned into the control loop so the translator
    // can surface decode-pipeline failures (`engine.decode_error`
    // notifications) to every connected UI client without round-tripping
    // through the event channel.
    let engine = EngineHandle::with_event_sink(event_tx.clone());
    // Wire the audio thread's master-bus limiter gain-reduction readout
    // into the bridge so every outgoing `engine.state_changed`
    // notification carries the live GR value for the UI meter.
    engine.attach_master_limiter_gr(stream.master_limiter_gr.clone());
    // Wire the SharedClock into the bridge so every outgoing
    // `engine.state_changed` notification carries the active
    // `clock_source` (Internal / MidiIn / AbletonLink) for the UI
    // BPM-lock badge. The MIDI clock-IN callback flips this byte on
    // 0xFA / 0xFC so the badge reacts to the master's transport.
    engine.attach_shared_clock(clock.shared.clone());
    // Wire the audio-thread perf counters into the bridge so every
    // outgoing `engine.state_changed` carries a fresh PerfSnapshot
    // (CPU%, render p99, underrun + dropped-frame counts) for the UI
    // perf dashboard. Refines the callback period from the device's
    // real sample rate (the io.rs probe defaults to a 512-frame
    // estimate at the seed-time sample rate; this set keeps the same
    // 512-frame heuristic but pins it to whatever the device picked).
    stream
        .perf
        .set_callback_period(hypehouse_engine::audio::PerfMetrics::callback_period_from(
            512,
            stream.sample_rate,
        ));
    engine.attach_perf_metrics(stream.perf.clone());

    // Control-thread loop runs on a dedicated OS thread so it doesn't
    // block the async runtime.
    let sample_rate = stream.sample_rate;
    let decode_for_control = Arc::clone(&decode_service);
    let shared_clock_for_control = clock.shared.clone();
    let engine_for_control = engine.clone();
    std::thread::spawn(move || {
        control_loop(
            event_rx,
            producer,
            clock,
            sample_rate,
            decode_for_control,
            shared_clock_for_control,
            event_log,
            engine_for_control,
        )
    });

    // Mid-stream decode-failure drain (PR #56 follow-up). The
    // decoder thread pushes onto a bounded sidechannel inside
    // `SymphoniaDecodeService`; this task polls the receiver and fans
    // out an `engine.decode_error` notification per event so corrupt
    // tracks / decoder-thread panics no longer silence the deck
    // invisibly. See `bridge::decode_drain` for cadence + capacity.
    let mid_stream_rx = decode_service.take_mid_stream_failure_receiver();
    if let Some(_drain) = bridge::spawn_decode_drain_if_some(engine.clone(), mid_stream_rx) {
        info!("decode-failure drain task started");
    } else {
        info!("decode-failure drain task skipped (service exposes no sidechannel)");
    }

    // One-shot auto-disengage sweeper (issue #118 final). Holds the
    // handle alive for the lifetime of `main` so the daemon thread
    // joins on graceful shutdown. Polls the engine snapshot at
    // ~50 Hz; idle when no one-shots are in flight.
    let _oneshot_sweeper = hypehouse_engine::oneshot_sweeper::spawn_oneshot_sweeper(engine.clone());
    info!("one-shot auto-disengage sweeper started");

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

    // Tear down the audio stream BEFORE finalizing the master.wav so
    // the cpal callback can't push more samples after we patch the
    // WAV header. Dropping the stream joins the OS audio thread.
    drop(stream);

    if let Some(rec) = master_recorder.as_mut() {
        let dropped = rec.dropped_frames();
        match rec.stop() {
            Ok(()) => info!(
                path = %rec.path().display(),
                dropped_frames = dropped,
                "master-mix recorder stopped + finalized"
            ),
            Err(e) => warn!(error = %e, "master-mix recorder stop failed"),
        }
    }

    Ok(())
}

/// Compute the on-disk path for the master-mix WAV. Mirrors the event
/// log layout: `<persistence root>/<session_id>/master.wav`. Honours
/// the same env override (`HYPEHOUSE_EVENT_LOG_DIR`) so a single env
/// var moves both the audit trail and the master mix to the same dir,
/// which is the most common ops deployment shape.
fn resolve_recording_path(session_id: &str) -> std::path::PathBuf {
    let root = std::env::var("HYPEHOUSE_EVENT_LOG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("XDG_DATA_HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|x| {
                    std::path::PathBuf::from(x)
                        .join("hypehouse-live")
                        .join("sessions")
                })
        })
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("hypehouse-live")
                .join("sessions")
        });
    root.join(session_id).join("master.wav")
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
#[allow(clippy::too_many_arguments)]
fn control_loop(
    event_rx: Receiver<Event>,
    mut producer: AudioProducer,
    clock: EngineClock,
    sample_rate: u32,
    decode: Arc<dyn DecodeService>,
    shared_clock: SharedClock,
    mut event_log: Option<EventLog>,
    engine: EngineHandle,
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
        let (cmds, decode_errors) = event_to_commands_with_errors(
            &state,
            &next,
            &ev,
            now_frame,
            sample_rate,
            decode.as_ref(),
        );
        for cmd in cmds.into_iter() {
            if let Err(dropped) = producer.try_push(cmd) {
                warn!(
                    ?dropped,
                    "audio ring full — dropping command (control plane backpressure)"
                );
            }
        }
        // Forward decode-pipeline failures (today: DeckLoad open errors)
        // to every connected WS client as an `engine.decode_error`
        // notification. Stringifying the underlying `DecodeError` keeps
        // the wire payload self-contained; the `category` field gives
        // the UI a stable key for toast styling.
        for failure in decode_errors {
            let category = failure.category();
            let error_text = format!("{}", failure.error);
            engine.publish_decode_error(failure.deck, failure.track_id, category, error_text);
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

/// Build the [`PeerClock`] backend per ADR-007 §v0.2.
///
/// Selection rules:
/// * Default → [`LinkStub`] (logs the "not yet wired" warning once).
/// * `ableton-link` feature ON + `ABLETON_LINK_ENABLED=1` → real
///   backend (currently panics on use — see `clock_sync::link_real`).
///   We intentionally do NOT construct it here at boot so a default
///   developer build with `ableton-link` accidentally enabled doesn't
///   crash on startup; the panic only surfaces when something actually
///   calls into the placeholder backend.
/// * Any other combination → [`LinkStub`].
fn build_peer_clock() -> Arc<dyn PeerClock> {
    #[cfg(feature = "ableton-link")]
    {
        let enabled = std::env::var("ABLETON_LINK_ENABLED")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if enabled {
            warn!(
                "ableton-link feature + ABLETON_LINK_ENABLED=1 — real backend is a placeholder; \
                 see ADR-007 §v0.2 + ADR-009 for the v0.2.x follow-up plan"
            );
            return Arc::new(hypehouse_engine::clock_sync::link_real::LinkReal::new(
                120.0,
            ));
        }
    }
    Arc::new(LinkStub::new())
}

/// Stringified backend label for the boot log. Pure helper so the log
/// line stays readable.
fn peer_clock_backend_label() -> &'static str {
    #[cfg(feature = "ableton-link")]
    {
        let enabled = std::env::var("ABLETON_LINK_ENABLED")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if enabled {
            return "real";
        }
    }
    "stub"
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

/// Spawn the MIDI clock IN listener if the user has configured it.
///
/// Selection rules (per ADR-007 §v0.3):
/// * Env var `MIDI_CLOCK_IN_DEVICE` unset / empty → disabled, returns `None`.
/// * `midi-clock-in` feature off at compile time → disabled even if env set.
/// * Substring match (case-insensitive) against the available input port
///   names. No match → log a warning, return `None`. We never fail the
///   whole engine on a missing MIDI device — DJ rigs commonly boot
///   without all the hardware plugged in.
fn spawn_midi_clock_in_if_enabled(
    shared_clock: SharedClock,
) -> Option<hypehouse_engine::midi::MidiClockIn> {
    let device = std::env::var("MIDI_CLOCK_IN_DEVICE").unwrap_or_default();
    let _ = shared_clock; // keep signature stable when feature is off
    if device.trim().is_empty() {
        info!("midi-clock-in: MIDI_CLOCK_IN_DEVICE unset — disabled");
        return None;
    }

    #[cfg(feature = "midi-clock-in")]
    {
        match hypehouse_engine::midi::MidiClockIn::start(Some(&device), shared_clock) {
            Ok(handle) => {
                info!(
                    port = %handle.port_name,
                    device_filter = %device,
                    "midi-clock-in: started — locking master_bpm to external MIDI clock"
                );
                Some(handle)
            }
            Err(e) => {
                warn!(
                    error = %e,
                    device_filter = %device,
                    "midi-clock-in: failed to open input port — continuing without"
                );
                None
            }
        }
    }
    #[cfg(not(feature = "midi-clock-in"))]
    {
        warn!(
            device_filter = %device,
            "midi-clock-in: env var set but feature `midi-clock-in` not enabled at compile time"
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
