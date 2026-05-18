//! Bridge-side drain for the decoder thread's mid-stream failure
//! sidechannel.
//!
//! PR #56 surfaced **open-time** decode errors as `engine.decode_error`
//! notifications (path missing, format unsupported, etc.). It left a
//! gap: errors the decoder thread observed **after** a successful
//! `open` — symphonia decode failures mid-track, rubato resample
//! failures, or panics in the decoder thread itself — silently padded
//! the SPSC ring with silence. From the operator's POV the deck just
//! went quiet for no observable reason.
//!
//! This module closes that gap. The decoder thread now pushes a
//! [`MidStreamFailure`] onto a bounded `crossbeam::channel`; this
//! drain task polls the channel on a 100ms cadence and broadcasts an
//! `engine.decode_error` notification for each one, reusing the same
//! [`BridgeNotice::DecodeError`] enum that PR #56 already defined so
//! the wire schema stays unchanged.
//!
//! ## Why not push directly from the audio thread?
//!
//! The audio thread is alloc-free + lock-free by ADR-004. It cannot
//! call `EngineHandle::publish_decode_error` (which allocates Strings
//! on every notification fan-out). The decoder thread *can* allocate,
//! but it runs per-track and shouldn't take a dep on the bridge
//! `EngineHandle`. The sidechannel is the cleanest decoupling — the
//! decoder thread owns a `Sender`, this task owns the `Receiver`.
//!
//! ## Channel + drain choices
//!
//! * **Capacity**: 64 events, defined in `audio::decode::MID_STREAM_FAILURE_CAPACITY`.
//!   Comfortably covers a multi-deck failure storm without losing
//!   events.
//! * **Backpressure**: `try_send` from the decoder thread — full
//!   channel drops + warns. The decoder thread must never block on a
//!   slow consumer (would freeze the audio pipeline).
//! * **Drain cadence**: 100 ms tick. Slow enough to avoid waking the
//!   tokio runtime needlessly, fast enough that a UI toast appears
//!   within human-perceptible latency.
//! * **Shutdown**: the task exits when the channel is disconnected
//!   (i.e. the `SymphoniaDecodeService` is dropped — typically on
//!   engine shutdown). No separate cancel channel needed.

use std::time::Duration;

use crossbeam::channel::{Receiver, TryRecvError};
use tokio::task::JoinHandle;

use crate::audio::{MidStreamFailure, MidStreamFailureKind};
use crate::bridge::EngineHandle;
use crate::state::DeckId;

/// Cadence between drain ticks. 100 ms picks an upper bound on
/// notification latency the UI can absorb without feeling laggy. Keep
/// in sync with `docs/api/ws-protocol.md` "drain cadence" note.
pub const DRAIN_TICK_MS: u64 = 100;

/// Convert a per-failure `deck` (a stable char so `audio::decode`
/// doesn't depend on `state::DeckId`) into the strongly-typed
/// `DeckId`. Unknown chars fall back to `DeckId::A` because the wire
/// `deck` field is non-nullable on the existing `DecodeError`
/// variant; choosing a default keeps the schema unchanged. The UI
/// already tolerates "deck unknown" via `category` + `track_id`.
fn deck_from_char(c: Option<char>) -> DeckId {
    match c {
        Some('B') | Some('b') => DeckId::B,
        _ => DeckId::A,
    }
}

/// Spawn the drain task on the supplied tokio runtime. Returns the
/// `JoinHandle` so `main.rs` can await it on shutdown if needed.
///
/// `engine` is cloned into the task (cheap — `Arc` inside). `rx` is
/// the receiver claimed via `DecodeService::take_mid_stream_failure_receiver`
/// at startup. If the caller has no receiver (e.g. a test stub
/// service that opts out of the sidechannel), `rx` will simply
/// receive no events and the task ticks silently — see
/// [`spawn_decode_drain_if_some`] for the optional convenience.
pub fn spawn_decode_drain(engine: EngineHandle, rx: Receiver<MidStreamFailure>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(DRAIN_TICK_MS));
        // `MissedTickBehavior::Skip` keeps us from bursting after a
        // long pause (e.g. the runtime was starved); we'd rather drop
        // a tick than fire several drains back-to-back.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if !drain_once(&engine, &rx) {
                // Channel disconnected — decoder service has shut down.
                tracing::info!(
                    target: "bridge.decode_drain",
                    "mid-stream failure channel closed; drain task exiting",
                );
                return;
            }
        }
    })
}

/// Convenience wrapper: only spawns the drain when the service hands
/// out a receiver. Returns `None` for stub services that opt out.
pub fn spawn_decode_drain_if_some(
    engine: EngineHandle,
    rx: Option<Receiver<MidStreamFailure>>,
) -> Option<JoinHandle<()>> {
    rx.map(|r| spawn_decode_drain(engine, r))
}

/// One drain iteration. Returns `false` iff the channel is
/// disconnected (the caller should exit). Pure helper — exposed for
/// unit tests so they can drive the drain without the tokio runtime.
pub fn drain_once(engine: &EngineHandle, rx: &Receiver<MidStreamFailure>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(failure) => publish(engine, failure),
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

/// Publish a single failure as an `engine.decode_error` notification.
fn publish(engine: &EngineHandle, failure: MidStreamFailure) {
    let category = failure.kind.category();
    let error_text = match &failure.kind {
        MidStreamFailureKind::DecodeFailed(s)
        | MidStreamFailureKind::ResampleFailed(s)
        | MidStreamFailureKind::ThreadPanic(s) => s.clone(),
    };
    let deck = deck_from_char(failure.deck);
    tracing::warn!(
        target: "bridge.decode_drain",
        track_id = %failure.track_id,
        category = %category,
        deck = ?deck,
        "publishing engine.decode_error from mid-stream sidechannel",
    );
    engine.publish_decode_error(deck, failure.track_id, category, error_text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{
        DecodeService, SymphoniaDecodeService, DECODER_THREAD_PANIC_CATEGORY, MID_STREAM_CATEGORY,
    };
    use crate::bridge::BridgeNotice;

    #[test]
    fn deck_from_char_maps_known_decks() {
        assert_eq!(deck_from_char(Some('A')), DeckId::A);
        assert_eq!(deck_from_char(Some('B')), DeckId::B);
        assert_eq!(deck_from_char(Some('b')), DeckId::B);
        // Unknown -> default A
        assert_eq!(deck_from_char(Some('Z')), DeckId::A);
        assert_eq!(deck_from_char(None), DeckId::A);
    }

    #[test]
    fn drain_once_publishes_decode_error_for_mid_stream_event() {
        let engine = EngineHandle::new();
        let mut subscriber = engine.subscribe();
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        svc.__inject_mid_stream_failure_for_test(MidStreamFailure {
            track_id: "trk-mid".into(),
            deck: Some('B'),
            kind: MidStreamFailureKind::DecodeFailed("synthetic corruption".into()),
        });
        let alive = drain_once(&engine, &rx);
        assert!(alive, "channel should still be alive (svc not dropped)");

        let notice = subscriber.try_recv().expect("notification queued");
        match notice {
            BridgeNotice::DecodeError {
                deck,
                track_id,
                category,
                error,
            } => {
                assert_eq!(deck, DeckId::B);
                assert_eq!(track_id, "trk-mid");
                assert_eq!(category, MID_STREAM_CATEGORY);
                assert!(error.contains("synthetic corruption"));
            }
            other => panic!("unexpected notice variant: {other:?}"),
        }
    }

    #[test]
    fn drain_once_publishes_decoder_thread_panic_category() {
        let engine = EngineHandle::new();
        let mut subscriber = engine.subscribe();
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        svc.__inject_mid_stream_failure_for_test(MidStreamFailure {
            track_id: "trk-panic".into(),
            deck: Some('A'),
            kind: MidStreamFailureKind::ThreadPanic("boom".into()),
        });
        assert!(drain_once(&engine, &rx));
        let notice = subscriber.try_recv().expect("notification queued");
        match notice {
            BridgeNotice::DecodeError {
                category, error, ..
            } => {
                assert_eq!(category, DECODER_THREAD_PANIC_CATEGORY);
                assert!(error.contains("boom"));
            }
            other => panic!("unexpected notice variant: {other:?}"),
        }
    }

    #[test]
    fn drain_once_returns_false_when_channel_disconnected() {
        let engine = EngineHandle::new();
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        drop(svc); // disconnects the sender
        let alive = drain_once(&engine, &rx);
        assert!(!alive, "drain_once must signal exit on disconnect");
    }

    #[test]
    fn drain_once_drains_multiple_events_in_one_tick() {
        let engine = EngineHandle::new();
        let mut subscriber = engine.subscribe();
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        for i in 0..5 {
            svc.__inject_mid_stream_failure_for_test(MidStreamFailure {
                track_id: format!("t-{i}"),
                deck: Some('A'),
                kind: MidStreamFailureKind::DecodeFailed(format!("err-{i}")),
            });
        }
        assert!(drain_once(&engine, &rx));
        // All five notifications must be queued — drain loops until
        // `try_recv` returns Empty.
        let mut seen = 0;
        while subscriber.try_recv().is_ok() {
            seen += 1;
        }
        assert_eq!(seen, 5);
    }
}
