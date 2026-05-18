//! Integration tests for the history RPCs (`engine.list_sessions` +
//! `engine.replay_session`).
//!
//! These exercise the dispatch surface — the same entry point the WS
//! server calls per inbound frame — against a real on-disk persistence
//! root populated with synthetic session directories. They guard the
//! contract documented in `docs/api/ws-protocol.md` so a refactor that
//! changes the JSON wire shape is caught immediately.

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use hypehouse_engine::bridge::rpc::{
    dispatch, method, EngineHandle, RpcId, RpcRequest, JSONRPC_VERSION,
};
use hypehouse_engine::persistence::ENV_LOG_DIR;
use hypehouse_engine::state::{DeckId, Event, EventKind, EventSource, TrackRef};
use serde_json::Value;

/// Tests in this file mutate `HYPEHOUSE_EVENT_LOG_DIR` — cargo runs them
/// in parallel by default, so we wrap each one in a process-wide mutex.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn scratch_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("hh-it-history-{tag}-{pid}-{nanos}"));
    fs::create_dir_all(&dir).expect("scratch dir create");
    dir
}

fn write_events(dir: &std::path::Path, events: &[Event]) {
    fs::create_dir_all(dir).expect("session dir");
    let mut f = File::create(dir.join("events.jsonl")).expect("create events");
    for e in events {
        let line = serde_json::to_string(e).expect("encode");
        writeln!(f, "{line}").expect("write line");
    }
}

fn req(id: i64, method_: &str, params: Value) -> RpcRequest {
    RpcRequest {
        jsonrpc: JSONRPC_VERSION.into(),
        method: method_.into(),
        params: Some(params),
        id: Some(RpcId::Num(id)),
    }
}

#[test]
fn list_sessions_then_replay_session_round_trip() {
    let _g = env_lock();
    let root = scratch_root("rt");
    std::env::set_var(ENV_LOG_DIR, &root);

    let sid = "20260301T000000Z-rt01";
    let events = [
        Event {
            id: 1,
            ts_micros: 100,
            source: EventSource::Ui,
            kind: EventKind::SessionStart,
        },
        Event {
            id: 2,
            ts_micros: 200,
            source: EventSource::Ui,
            kind: EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "tr-1".into(),
                    path: "/m/tr-1.mp3".into(),
                },
                bpm: 130.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![0, 1846, 3692],
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        },
        Event {
            id: 3,
            ts_micros: 300,
            source: EventSource::Ui,
            kind: EventKind::DeckPlay { deck: DeckId::A },
        },
    ];
    write_events(&root.join(sid), &events);
    // Drop a non-empty master.wav so `has_recording` flips to true.
    fs::write(root.join(sid).join("master.wav"), b"RIFFfakeWAVEdata").expect("write master.wav");

    let engine = EngineHandle::new();

    // list_sessions surfaces the session with the right metadata.
    let resp = dispatch(&engine, req(1, method::LIST_SESSIONS, Value::Null));
    assert!(
        resp.error.is_none(),
        "list_sessions error: {:?}",
        resp.error
    );
    let result = resp.result.expect("result");
    let sessions = result.get("sessions").and_then(Value::as_array).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], serde_json::json!(sid));
    assert_eq!(sessions[0]["event_count"], serde_json::json!(3));
    assert_eq!(sessions[0]["started_at_micros"], serde_json::json!(100));
    assert_eq!(sessions[0]["ended_at_micros"], serde_json::json!(300));
    assert_eq!(sessions[0]["has_recording"], serde_json::json!(true));

    // replay_session reconstructs the deck state from the events.
    let resp = dispatch(
        &engine,
        req(
            2,
            method::REPLAY_SESSION,
            serde_json::json!({ "session_id": sid }),
        ),
    );
    assert!(
        resp.error.is_none(),
        "replay_session error: {:?}",
        resp.error
    );
    let result = resp.result.expect("result");
    assert_eq!(result["event_count"], serde_json::json!(3));
    let state = &result["state"];
    assert_eq!(state["session_active"], serde_json::json!(true));
    let deck_a = &state["deck_a"];
    assert_eq!(deck_a["playing"], serde_json::json!(true));
    let bpm = deck_a["bpm"].as_f64().unwrap();
    assert!((bpm - 130.0).abs() < 1e-3, "bpm not 130: {bpm}");

    std::env::remove_var(ENV_LOG_DIR);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn list_sessions_handles_corrupt_and_missing_logs_gracefully() {
    let _g = env_lock();
    let root = scratch_root("graceful");
    std::env::set_var(ENV_LOG_DIR, &root);

    // Session A: well-formed.
    let sid_ok = "20260301T010101Z-ok01";
    write_events(
        &root.join(sid_ok),
        &[Event {
            id: 1,
            ts_micros: 1_000,
            source: EventSource::Ui,
            kind: EventKind::SessionStart,
        }],
    );
    // Session B: dir exists but no events file.
    let sid_no_file = "20260301T010101Z-nofi";
    fs::create_dir_all(root.join(sid_no_file)).expect("dir");
    // Session C: events file present but garbled.
    let sid_corrupt = "20260301T010101Z-cor1";
    fs::create_dir_all(root.join(sid_corrupt)).expect("dir");
    fs::write(
        root.join(sid_corrupt).join("events.jsonl"),
        b"this is not json\nstill not json\n",
    )
    .expect("write corrupt");

    let engine = EngineHandle::new();
    let resp = dispatch(&engine, req(1, method::LIST_SESSIONS, Value::Null));
    assert!(
        resp.error.is_none(),
        "list_sessions error: {:?}",
        resp.error
    );
    let result = resp.result.expect("result");
    let sessions = result.get("sessions").and_then(Value::as_array).unwrap();
    assert_eq!(sessions.len(), 3, "three sessions: {sessions:?}");

    // The well-formed session surfaces a timestamp; the others fall back.
    let ok = sessions
        .iter()
        .find(|s| s["id"] == serde_json::json!(sid_ok))
        .expect("ok session present");
    assert_eq!(ok["started_at_micros"], serde_json::json!(1_000));
    let nofi = sessions
        .iter()
        .find(|s| s["id"] == serde_json::json!(sid_no_file))
        .expect("nofile session present");
    assert!(nofi["started_at_micros"].is_null());
    assert_eq!(nofi["event_count"], serde_json::json!(0));
    let cor = sessions
        .iter()
        .find(|s| s["id"] == serde_json::json!(sid_corrupt))
        .expect("corrupt session present");
    assert_eq!(cor["event_count"], serde_json::json!(2));
    assert!(cor["started_at_micros"].is_null());

    // replay_session on the corrupt log surfaces an error (bad JSON
    // can't fold), but the response is shaped (no panic, no hang).
    let resp = dispatch(
        &engine,
        req(
            2,
            method::REPLAY_SESSION,
            serde_json::json!({ "session_id": sid_corrupt }),
        ),
    );
    assert!(resp.error.is_some(), "expected error on corrupt log");

    // replay_session on the no-file session returns default state.
    let resp = dispatch(
        &engine,
        req(
            3,
            method::REPLAY_SESSION,
            serde_json::json!({ "session_id": sid_no_file }),
        ),
    );
    assert!(resp.error.is_none(), "expected ok on missing log");
    let result = resp.result.expect("result");
    assert_eq!(result["event_count"], serde_json::json!(0));

    std::env::remove_var(ENV_LOG_DIR);
    fs::remove_dir_all(&root).ok();
}
