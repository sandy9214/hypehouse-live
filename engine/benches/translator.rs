//! Criterion bench for `event_to_commands` (ADR-004).
//!
//! Gate: p99 must be < 50 µs. The translator runs on the control thread,
//! not the audio thread, but it still sits on the hot path for every UI
//! / MIDI / co-pilot event — sluggish translation = sluggish response.

use criterion::{criterion_group, criterion_main, Criterion};
use hypehouse_engine::audio::{event_to_commands, StubDecodeService};
use hypehouse_engine::state::{
    DeckId, EngineState, EqBand, Event, EventKind, EventSource, TrackRef,
};
use std::hint::black_box;

fn ev(id: u64, kind: EventKind) -> Event {
    Event {
        id,
        ts_micros: 0,
        source: EventSource::Ui,
        kind,
    }
}

fn bench_event_to_commands(c: &mut Criterion) {
    let sample_rate: u32 = 48_000;
    let decode = StubDecodeService::new();
    let prev = EngineState::default();

    let e_play = ev(1, EventKind::DeckPlay { deck: DeckId::A });
    let next_play = prev.apply(&e_play);

    let e_xfade = ev(2, EventKind::Crossfader { value: 0.75 });
    let next_xfade = prev.apply(&e_xfade);

    let e_eq = ev(
        3,
        EventKind::EqAdjust {
            deck: DeckId::A,
            band: EqBand::Low,
            value_db: -6.0,
        },
    );
    let next_eq = prev.apply(&e_eq);

    let e_takeover = ev(
        4,
        EventKind::TakeOver {
            deck: DeckId::A,
            handoff_until_frame: 96_000,
        },
    );
    let next_takeover = prev.apply(&e_takeover);

    let e_load = ev(
        5,
        EventKind::DeckLoad {
            deck: DeckId::A,
            track: TrackRef {
                id: "song-1".into(),
                path: "/p".into(),
            },
            bpm: 128.0,
            beat_grid_anchor_ms: 0,
            downbeats_ms: vec![],
            hot_cues: [None; 8],
            track_gain_db: 0.0,
        },
    );
    let next_load = prev.apply(&e_load);

    c.bench_function("translator::deck_play", |b| {
        b.iter(|| {
            black_box(event_to_commands(
                black_box(&prev),
                black_box(&next_play),
                black_box(&e_play),
                black_box(0),
                black_box(sample_rate),
                black_box(&decode),
            ))
        })
    });

    c.bench_function("translator::crossfader", |b| {
        b.iter(|| {
            black_box(event_to_commands(
                black_box(&prev),
                black_box(&next_xfade),
                black_box(&e_xfade),
                black_box(0),
                black_box(sample_rate),
                black_box(&decode),
            ))
        })
    });

    c.bench_function("translator::eq_low", |b| {
        b.iter(|| {
            black_box(event_to_commands(
                black_box(&prev),
                black_box(&next_eq),
                black_box(&e_eq),
                black_box(0),
                black_box(sample_rate),
                black_box(&decode),
            ))
        })
    });

    c.bench_function("translator::takeover", |b| {
        b.iter(|| {
            black_box(event_to_commands(
                black_box(&prev),
                black_box(&next_takeover),
                black_box(&e_takeover),
                black_box(0),
                black_box(sample_rate),
                black_box(&decode),
            ))
        })
    });

    // DeckLoad hits the stub decode service; we pre-warm so we don't
    // benchmark sine-wave generation in the steady state.
    let _ = event_to_commands(&prev, &next_load, &e_load, 0, sample_rate, &decode);
    c.bench_function("translator::deck_load_cached", |b| {
        b.iter(|| {
            black_box(event_to_commands(
                black_box(&prev),
                black_box(&next_load),
                black_box(&e_load),
                black_box(0),
                black_box(sample_rate),
                black_box(&decode),
            ))
        })
    });
}

criterion_group!(benches, bench_event_to_commands);
criterion_main!(benches);
