//! Integration test for the WebSocket bridge.
//!
//! Spins up the server on an ephemeral loopback port, connects a real
//! `tokio-tungstenite` client, submits an event, asserts the
//! `engine.state_changed` notification arrives and that `engine.snapshot`
//! reflects the change.

use crossbeam::channel;
use futures_util::{SinkExt, StreamExt};
use hypehouse_engine::bridge::{spawn, AuthConfig, BridgeConfig, EngineHandle};
use hypehouse_engine::state::Event;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio_tungstenite::tungstenite::Message;

fn ephemeral_loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_submit_event_notifies_and_snapshot_reflects() {
    // Wire an event sink so `submit_event` flows through the control-loop
    // channel (matches production wiring in `main.rs`). A small helper
    // thread plays the role of the control loop: it drains events and
    // re-applies them onto the bridge's `EngineHandle` so the
    // `state_changed` broadcast fan-out still runs.
    let (event_tx, event_rx) = channel::unbounded::<Event>();
    let engine = EngineHandle::with_event_sink(event_tx);
    let engine_for_loop = engine.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event_rx.recv() {
            engine_for_loop.submit_event_kind(ev.kind, ev.source);
        }
    });

    let cfg = BridgeConfig {
        bind_addr: ephemeral_loopback(),
        auth: AuthConfig::default(),
    };
    let server = spawn(cfg, engine.clone()).await.expect("spawn bridge");
    let url = format!("ws://{}", server.local_addr);

    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");

    // Submit a DeckPlay for deck A.
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "engine.submit_event",
        "params": { "kind": { "DeckPlay": { "deck": "A" } } },
        "id": 42
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let mut saw_response = false;
    let mut saw_notify = false;
    let mut last_event_id = 0u64;

    for _ in 0..6 {
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("id").and_then(|x| x.as_i64()) == Some(42) {
            assert_eq!(v["result"]["accepted"], serde_json::Value::Bool(true));
            saw_response = true;
        } else if v.get("method").and_then(|x| x.as_str()) == Some("engine.state_changed") {
            last_event_id = v["params"]["last_event_id"].as_u64().unwrap();
            assert!(v["params"]["state"]["deck_a"]["playing"].as_bool().unwrap());
            saw_notify = true;
        }
        if saw_response && saw_notify {
            break;
        }
    }
    assert!(saw_response, "missing JSON-RPC response");
    assert!(saw_notify, "missing state_changed notification");
    assert!(last_event_id >= 1, "last_event_id should be set");

    // Verify snapshot via RPC.
    let snap = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "engine.snapshot",
        "id": 99
    });
    ws.send(Message::Text(snap.to_string().into()))
        .await
        .unwrap();
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("id").and_then(|x| x.as_i64()) == Some(99) {
            assert!(v["result"]["deck_a"]["playing"].as_bool().unwrap());
            break;
        }
    }

    // Health check via RPC has the expected keys.
    let hreq = serde_json::json!({ "jsonrpc": "2.0", "method": "engine.health", "id": 100 });
    ws.send(Message::Text(hreq.to_string().into()))
        .await
        .unwrap();
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v.get("id").and_then(|x| x.as_i64()) == Some(100) {
            for key in [
                "uptime_ms",
                "audio_xrun_count",
                "ws_clients_connected",
                "ring_pending",
            ] {
                assert!(v["result"].get(key).is_some(), "missing health key {key}");
            }
            break;
        }
    }

    ws.close(None).await.ok();
    server.shutdown().await.expect("shutdown clean");
}
