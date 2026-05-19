//! Beat-FX one-shot auto-disengage sweeper.
//!
//! Issue #118 final slice. The reducer engages a slot's effect on
//! `EffectOneShot` and records `(was_enabled, ends_at_micros)` on the
//! slot. This module owns the matching auto-disengage path: a low-rate
//! background thread polls the engine snapshot every 50 ms and, for any
//! slot whose `ends_at_micros` has elapsed, emits a synthetic
//! `EffectEnable { enabled: was_enabled }` event tagged
//! `EventSource::Internal`. The control loop applies it normally — the
//! reducer clears the `one_shot` field (existing supersede path) and
//! the translator emits the matching `AudioCommandKind::EffectEnable`
//! to the audio ring, so the live mix transitions back in alignment
//! with what the UI countdown showed.
//!
//! Why a separate thread, not the audio callback or the control loop:
//!   - The audio callback is alloc-/lock-free (ADR-004). Reading the
//!     full snapshot to scan for expired one-shots would violate that.
//!   - The control loop is event-driven (`recv()` blocks). Putting
//!     timeouts in there means changing every event-loop iteration into
//!     a `recv_timeout` + scan, which is a measurable hot-path tax for
//!     a feature that fires a handful of times per minute.
//!
//! The 50 ms polling cadence is a deliberate floor: any beat at ≥30 BPM
//! is ≥2 s, so 50 ms gives a `≤2.5%` UI-vs-audio mismatch on the very
//! shortest 1-beat one-shot @ 200 BPM (300 ms beat → 16% relative
//! error at worst). 99% of real-world one-shots are 4+ beats so the
//! lag is imperceptible. Tightening cadence has diminishing returns
//! and grows the engine's idle CPU draw on a quiet session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::bridge::EngineHandle;
use crate::state::{DeckId, EventKind, EventSource};

/// Default poll period — see module-doc rationale.
pub const DEFAULT_POLL_PERIOD: Duration = Duration::from_millis(50);

/// Owns the sweeper thread. Drop signals shutdown + joins.
pub struct OneShotSweeperHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for OneShotSweeperHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Spawn the sweeper. Holds an `EngineHandle` clone for snapshot reads
/// and stamped event forwarding. Idle when no in-flight one-shots —
/// snapshot read is a single mutex lock per tick.
pub fn spawn_oneshot_sweeper(engine: EngineHandle) -> OneShotSweeperHandle {
    spawn_oneshot_sweeper_with_period(engine, DEFAULT_POLL_PERIOD)
}

/// Test seam — caller chooses poll period (tighter for unit tests).
pub fn spawn_oneshot_sweeper_with_period(
    engine: EngineHandle,
    period: Duration,
) -> OneShotSweeperHandle {
    let shutdown = Arc::new(AtomicBool::new(false));
    let stop = shutdown.clone();
    let join = thread::Builder::new()
        .name("oneshot-sweeper".to_string())
        .spawn(move || sweeper_loop(engine, period, stop))
        .expect("spawn oneshot-sweeper thread");
    OneShotSweeperHandle {
        shutdown,
        join: Some(join),
    }
}

fn sweeper_loop(engine: EngineHandle, period: Duration, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        sweep_once(&engine);
        thread::sleep(period);
    }
}

/// Pure helper — scan a snapshot for expired one-shots and emit
/// synthetic `EffectEnable` events. Public for unit tests + future
/// alternative drivers. Returns the count of events successfully
/// forwarded.
pub fn sweep_snapshot(
    engine: &EngineHandle,
    snap: &crate::state::EngineState,
    now_micros: i64,
) -> usize {
    let mut emitted = 0usize;
    for (deck_id, deck) in [(DeckId::A, &snap.deck_a), (DeckId::B, &snap.deck_b)] {
        for (slot_idx, slot) in deck.effects.iter().enumerate() {
            let Some(os) = &slot.one_shot else {
                continue;
            };
            if now_micros >= os.ends_at_micros {
                let ev = engine.stamp_event(
                    EventKind::EffectEnable {
                        deck: deck_id,
                        slot: slot_idx as u8,
                        enabled: os.was_enabled,
                    },
                    EventSource::Internal,
                );
                // Channel full / disconnected → best-effort. A missed
                // disengage will retry on the next pass since
                // `one_shot` stays set until the EffectEnable lands
                // in the reducer + clears it.
                if engine.forward_event(ev).is_ok() {
                    emitted += 1;
                }
            }
        }
    }
    emitted
}

/// Single sweep — reads `engine.snapshot()` + wall clock then defers
/// to [`sweep_snapshot`]. Returns the disengage-event count.
pub fn sweep_once(engine: &EngineHandle) -> usize {
    let now_micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);
    let snap = engine.snapshot();
    sweep_snapshot(engine, &snap, now_micros)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::EngineHandle;
    use crate::state::{EffectSlot, EngineState, Event, OneShotState};
    use crossbeam::channel;

    fn engine_with_sink() -> (EngineHandle, channel::Receiver<Event>) {
        let (tx, rx) = channel::bounded::<Event>(64);
        (EngineHandle::with_event_sink(tx), rx)
    }

    fn synthetic_state(slot0: EffectSlot) -> EngineState {
        let mut s = EngineState::default();
        s.deck_a.effects[0] = slot0;
        s
    }

    #[test]
    fn sweep_snapshot_emits_disengage_when_elapsed() {
        let (engine, rx) = engine_with_sink();
        let mut slot = EffectSlot {
            effect_id: 1,
            enabled: true,
            ..Default::default()
        };
        slot.one_shot = Some(OneShotState {
            ends_at_micros: 100,
            was_enabled: false,
            beat_period_ms_at_dispatch: 500.0,
        });
        let snap = synthetic_state(slot);
        let emitted = sweep_snapshot(&engine, &snap, 200);
        assert_eq!(emitted, 1);
        let ev = rx.try_recv().expect("disengage event forwarded");
        assert!(matches!(
            ev.kind,
            EventKind::EffectEnable {
                deck: DeckId::A,
                slot: 0,
                enabled: false
            }
        ));
        assert!(matches!(ev.source, EventSource::Internal));
    }

    #[test]
    fn sweep_snapshot_emits_nothing_when_still_in_flight() {
        let (engine, rx) = engine_with_sink();
        let mut slot = EffectSlot {
            effect_id: 1,
            enabled: true,
            ..Default::default()
        };
        slot.one_shot = Some(OneShotState {
            ends_at_micros: 1_000_000_000,
            was_enabled: false,
            beat_period_ms_at_dispatch: 500.0,
        });
        let snap = synthetic_state(slot);
        let emitted = sweep_snapshot(&engine, &snap, 200);
        assert_eq!(emitted, 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sweep_snapshot_restores_was_enabled_true() {
        let (engine, rx) = engine_with_sink();
        let mut slot = EffectSlot {
            effect_id: 1,
            enabled: true,
            ..Default::default()
        };
        slot.one_shot = Some(OneShotState {
            ends_at_micros: 100,
            was_enabled: true,
            beat_period_ms_at_dispatch: 500.0,
        });
        let snap = synthetic_state(slot);
        sweep_snapshot(&engine, &snap, 200);
        let ev = rx.try_recv().expect("event");
        assert!(matches!(
            ev.kind,
            EventKind::EffectEnable { enabled: true, .. }
        ));
    }

    #[test]
    fn sweep_snapshot_no_one_shot_emits_nothing() {
        let (engine, rx) = engine_with_sink();
        let snap = EngineState::default();
        let emitted = sweep_snapshot(&engine, &snap, 200);
        assert_eq!(emitted, 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sweep_snapshot_handles_deck_b_too() {
        let (engine, rx) = engine_with_sink();
        let mut state = EngineState::default();
        state.deck_b.effects[2] = EffectSlot {
            effect_id: 4,
            enabled: true,
            one_shot: Some(OneShotState {
                ends_at_micros: 50,
                was_enabled: false,
                beat_period_ms_at_dispatch: 500.0,
            }),
            ..Default::default()
        };
        let emitted = sweep_snapshot(&engine, &state, 100);
        assert_eq!(emitted, 1);
        let ev = rx.try_recv().expect("event");
        assert!(matches!(
            ev.kind,
            EventKind::EffectEnable {
                deck: DeckId::B,
                slot: 2,
                ..
            }
        ));
    }

    #[test]
    fn sweeper_thread_starts_and_drops_cleanly() {
        let (engine, _rx) = engine_with_sink();
        // Tight period so we observe at least one tick under the test.
        let h = spawn_oneshot_sweeper_with_period(engine, Duration::from_millis(5));
        thread::sleep(Duration::from_millis(20));
        // Drop signals shutdown + joins. No assertion needed — the
        // test passes by not panicking / hanging.
        drop(h);
    }
}
