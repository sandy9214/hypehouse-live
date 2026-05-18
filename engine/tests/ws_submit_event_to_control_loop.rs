//! End-to-end: UI → WS bridge → `submit_event` → control-loop event
//! channel.
//!
//! Spins the bridge with `EngineHandle::with_event_sink(tx)`, connects a
//! WS client, calls `engine.submit_event` with a `DeckLoad` payload, and
//! asserts the matching `Event` lands on the channel receiver — the same
//! handoff `engine/src/main.rs` uses for the real control loop.
//!
//! Covers the wiring promised in the PR: `feat(engine): wire WS
//! submit_event → control-loop event channel`. Companion to the unit
//! tests in `engine/src/bridge/rpc.rs` (full / disconnected / unwired
//! channel error paths) and the broader fan-out test in
//! `engine/tests/ws_bridge_integration.rs`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use crossbeam::channel;
use futures_util::{SinkExt, StreamExt};
use hypehouse_engine::bridge::{spawn, AuthConfig, BridgeConfig, EngineHandle};
use hypehouse_engine::state::{DeckId, Event, EventKind, EventSource};
use tokio_tungstenite::tungstenite::Message;

fn ephemeral_loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

/// Wait for a JSON-RPC response with the given `id`, with a short
/// timeout. Returns the parsed value or panics with context.
async fn read_response_with_id(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    target_id: i64,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .expect("timed out waiting for response")
            .expect("ws stream ended")
            .expect("ws frame error");
        if let Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v.get("id").and_then(|x| x.as_i64()) == Some(target_id) {
                return v;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_submit_event_deck_load_forwards_to_control_loop_channel() {
    let (event_tx, event_rx) = channel::unbounded::<Event>();
    let engine = EngineHandle::with_event_sink(event_tx);
    let cfg = BridgeConfig {
        bind_addr: ephemeral_loopback(),
        auth: AuthConfig::default(),
    };
    let server = spawn(cfg, engine.clone()).await.expect("spawn bridge");
    let url = format!("ws://{}", server.local_addr);

    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");

    // Submit a DeckLoad — picked because it carries multiple fields
    // (deck + track + bpm + beat_grid_anchor_ms) so a wire-shape regression
    // surfaces here.
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "engine.submit_event",
        "params": {
            "kind": {
                "DeckLoad": {
                    "deck": "A",
                    "track": { "id": "trk-1", "path": "/music/song.mp3" },
                    "bpm": 128.0,
                    "beat_grid_anchor_ms": 12
                }
            }
        },
        "id": 1
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let resp = read_response_with_id(&mut ws, 1).await;
    assert!(resp.get("error").is_none(), "unexpected error: {resp:?}");
    assert_eq!(resp["result"]["accepted"], serde_json::Value::Bool(true));

    // The control-loop receiver should now hold exactly the event we sent.
    let ev = event_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("event landed on channel");
    match ev.kind {
        EventKind::DeckLoad {
            deck: DeckId::A,
            track,
            bpm,
            beat_grid_anchor_ms,
            downbeats_ms,
            hot_cues,
        } => {
            assert_eq!(track.id, "trk-1");
            assert_eq!(track.path, "/music/song.mp3");
            assert!((bpm - 128.0).abs() < f32::EPSILON);
            assert_eq!(beat_grid_anchor_ms, 12);
            // Wire payload omitted `downbeats_ms`; serde default = [].
            assert!(downbeats_ms.is_empty());
            // Wire payload omitted `hot_cues`; serde default = all None.
            assert!(hot_cues.iter().all(Option::is_none));
        }
        other => panic!("unexpected event kind on channel: {other:?}"),
    }
    // Default source for bare/wrapped(no source) is Ui per dispatch contract.
    assert!(matches!(ev.source, EventSource::Ui));
    // The bridge stamps a monotonic id starting at 1.
    assert!(ev.id >= 1, "expected stamped id >=1, got {}", ev.id);

    ws.close(None).await.ok();
    server.shutdown().await.expect("shutdown clean");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_submit_event_returns_engine_offline_when_channel_full() {
    // Bounded channel of capacity 1024 — the 1025th submit must trip
    // `-32000 engine offline`. We never drain the receiver so back-pressure
    // is forced.
    let (tx, _rx) = channel::bounded::<Event>(1024);
    let engine = EngineHandle::with_event_sink(tx);
    let cfg = BridgeConfig {
        bind_addr: ephemeral_loopback(),
        auth: AuthConfig::default(),
    };
    let server = spawn(cfg, engine.clone()).await.expect("spawn bridge");
    let url = format!("ws://{}", server.local_addr);

    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");

    // Fire 1100 events.
    for i in 1..=1100i64 {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "engine.submit_event",
            "params": { "kind": { "DeckPlay": { "deck": "A" } } },
            "id": i,
        });
        ws.send(Message::Text(req.to_string().into()))
            .await
            .unwrap();
    }

    let mut accepted = 0usize;
    let mut engine_offline = 0usize;
    for i in 1..=1100i64 {
        let v = read_response_with_id(&mut ws, i).await;
        if let Some(err) = v.get("error") {
            if err.get("code").and_then(|c| c.as_i64()) == Some(-32000) {
                engine_offline += 1;
            } else {
                panic!("unexpected error response at id={i}: {err:?}");
            }
        } else {
            assert_eq!(v["result"]["accepted"], serde_json::Value::Bool(true));
            accepted += 1;
        }
    }

    // First 1024 should fit; the remaining 76 must surface engine_offline.
    assert_eq!(accepted, 1024, "expected exactly 1024 accepted submits");
    assert_eq!(
        engine_offline, 76,
        "expected exactly 76 engine_offline responses",
    );

    ws.close(None).await.ok();
    server.shutdown().await.expect("shutdown clean");
}
