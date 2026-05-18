//! Measure append latency for the persistent event log so the PR
//! description can quote real numbers. Not a Criterion benchmark —
//! Criterion takes 15+ seconds and CI doesn't need that resolution.
//! `cargo test --release event_log_append_latency -- --nocapture`
//! prints `median_ns p99_ns` to stdout. The test asserts a generous
//! upper bound (median < 100 µs, p99 < 1 ms) so a regression on a
//! reasonable disk surfaces; the absolute number is what we quote.

use hypehouse_engine::persistence::{EventLog, ENV_LOG_DIR};
use hypehouse_engine::state::{DeckId, Event, EventKind, EventSource};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[test]
fn measure_append_latency_1000_events() {
    let _g = test_lock();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("hh-evlog-bench-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    std::env::set_var(ENV_LOG_DIR, &dir);
    std::env::remove_var("HYPEHOUSE_EVENT_LOG_DISABLED");

    let mut log = EventLog::new("latency-bench").expect("open");
    let mut samples: Vec<u128> = Vec::with_capacity(1000);

    // Warm: open file + first write often allocates.
    log.append(&mk(0, EventKind::SessionStart)).expect("warm");

    for i in 1..=1000u64 {
        let ev = mk(
            i,
            EventKind::Crossfader {
                value: (i as f32) / 1000.0,
            },
        );
        let t = Instant::now();
        log.append(&ev).expect("append");
        samples.push(t.elapsed().as_nanos());
    }
    drop(log);

    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let p99 = samples[(samples.len() * 99) / 100];
    let max = *samples.last().unwrap();

    eprintln!(
        "event_log append latency over {} samples: median={}ns p99={}ns max={}ns",
        samples.len(),
        median,
        p99,
        max,
    );
    // The point of this test is the printout; the asserts are a soft
    // floor so a 100x regression on macOS/Linux/Windows surfaces.
    assert!(median < 100_000, "median {median}ns > 100µs");
    assert!(p99 < 1_000_000, "p99 {p99}ns > 1ms");
    std::fs::remove_dir_all(&dir).ok();
}

fn mk(id: u64, kind: EventKind) -> Event {
    Event {
        id,
        ts_micros: id as i64,
        source: EventSource::Ui,
        kind,
    }
}

#[allow(dead_code)]
fn touch_deck_id_to_silence_unused() {
    let _ = DeckId::A;
}
