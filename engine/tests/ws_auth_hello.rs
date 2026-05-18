//! Integration tests for the in-band `auth.hello` JSON-RPC handshake.
//!
//! Browsers cannot attach an `Authorization: Bearer …` header to a
//! WebSocket upgrade, so the engine accepts header-less connections in a
//! `PendingAuth` state. The client must call `auth.hello` as its very
//! first JSON-RPC method; everything else short-circuits with
//! `AUTH_REJECTED`. A 5-second idle timeout (per
//! `ws_server::PENDING_AUTH_TIMEOUT`) closes silent clients with WS
//! close code 1008.
//!
//! These tests cover the **observable contract** from the wire: connect
//! without a header → server accepts; gated methods reject; valid
//! handshake unlocks the connection; idle pending-auth gets evicted.

use crossbeam::channel;
use futures_util::{SinkExt, StreamExt};
use hypehouse_engine::bridge::{spawn, AuthConfig, BridgeConfig, BridgeServer, EngineHandle};
use hypehouse_engine::state::Event;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;

fn ephemeral_loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

/// Spin up a bridge with the supplied auth + a tiny control-loop shim.
///
/// Post-PR-#7 `engine.submit_event` requires a wired event sink: it
/// forwards onto the channel rather than mutating state directly, and
/// returns `-32001 ENGINE_SINK_UNWIRED` without one. The shim drains
/// events back into the same `EngineHandle` so the rest of the bridge
/// (state, snapshot, notifications) keeps behaving like production.
///
/// Returns `(server, event_drain_rx)` — the receiver clone lets tests
/// assert which `Event`s landed in the control-loop slot.
async fn start_bridge(auth: AuthConfig) -> (BridgeServer, channel::Receiver<Event>) {
    let (event_tx, event_rx) = channel::unbounded::<Event>();
    let engine = EngineHandle::with_event_sink(event_tx);
    let engine_for_loop = engine.clone();
    let (drain_tx, drain_rx) = channel::unbounded::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = event_rx.recv() {
            // Forward to the assertions channel first (cheap clone — the
            // event is small) so tests can observe forwarding even when
            // the bridge race-loses the broadcast.
            let _ = drain_tx.send(ev.clone());
            engine_for_loop.submit_event_kind(ev.kind, ev.source);
        }
    });
    let cfg = BridgeConfig {
        bind_addr: ephemeral_loopback(),
        auth,
    };
    let server = spawn(cfg, engine).await.expect("spawn bridge");
    (server, drain_rx)
}

/// Pull the next JSON-RPC envelope addressed to `id` from the stream,
/// draining any unrelated state-change notifications along the way.
async fn await_response<S>(ws: &mut S, want_id: i64) -> serde_json::Value
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
        match next {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).expect("text frame is JSON");
                if v.get("id").and_then(|x| x.as_i64()) == Some(want_id) {
                    return v;
                }
                // Likely a state_changed notification — keep draining.
            }
            Ok(Some(Ok(_other))) => continue,
            Ok(Some(Err(e))) => panic!("ws read error: {e}"),
            Ok(None) => panic!("ws closed before response id={want_id}"),
            Err(_) => panic!("timed out waiting for response id={want_id}"),
        }
    }
    panic!("hard deadline waiting for response id={want_id}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_mode_connects_without_authorization_header() {
    // No header at upgrade → server still accepts; auth.hello will gate
    // every subsequent method until it runs.
    let (server, _drain) = start_bridge(AuthConfig::with_token("s3cret")).await;
    let url = format!("ws://{}", server.local_addr);

    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("plain connect must succeed; the upgrade is allowed");

    ws.close(None).await.ok();
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_event_before_auth_hello_is_rejected() {
    let (server, drain) = start_bridge(AuthConfig::with_token("s3cret")).await;
    let url = format!("ws://{}", server.local_addr);

    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    // Try a state-mutating method while still in PendingAuth.
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "engine.submit_event",
        "params": { "kind": { "DeckPlay": { "deck": "A" } } },
        "id": 1
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let resp = await_response(&mut ws, 1).await;
    assert_eq!(
        resp["error"]["code"].as_i64().unwrap(),
        hypehouse_engine::bridge::AUTH_REJECTED as i64,
        "expected AUTH_REJECTED, got: {resp}"
    );
    // Engine never received the event — gate is before forward_event.
    assert!(
        drain.try_recv().is_err(),
        "no event should have reached the control loop"
    );

    ws.close(None).await.ok();
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_hello_then_submit_event_succeeds() {
    let (server, drain) = start_bridge(AuthConfig::with_token("s3cret")).await;
    let url = format!("ws://{}", server.local_addr);

    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    // Step 1: auth.hello.
    let hello = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "auth.hello",
        "params": { "token": "s3cret" },
        "id": 10
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    let resp = await_response(&mut ws, 10).await;
    assert!(
        resp.get("error").is_none() || resp["error"].is_null(),
        "auth.hello with valid token must succeed: {resp}"
    );
    assert_eq!(resp["result"]["authed"], serde_json::Value::Bool(true));
    assert!(resp["result"].get("session").is_some());

    // Step 2: submit_event now allowed.
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "engine.submit_event",
        "params": { "kind": { "DeckPlay": { "deck": "A" } } },
        "id": 11
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();
    let resp = await_response(&mut ws, 11).await;
    assert!(resp.get("error").is_none() || resp["error"].is_null());
    assert_eq!(resp["result"]["accepted"], serde_json::Value::Bool(true));

    // Event reached the control-loop shim.
    let ev = drain
        .recv_timeout(Duration::from_secs(2))
        .expect("forwarded event must arrive");
    assert!(
        matches!(
            ev.kind,
            hypehouse_engine::state::EventKind::DeckPlay {
                deck: hypehouse_engine::state::DeckId::A
            }
        ),
        "unexpected kind: {:?}",
        ev.kind
    );

    ws.close(None).await.ok();
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_auth_idle_timeout_closes_with_code_1008() {
    let (server, _drain) = start_bridge(AuthConfig::with_token("s3cret")).await;
    let url = format!("ws://{}", server.local_addr);

    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    // Don't send anything. The 5s pending-auth timer in ws_server fires
    // and the server sends a 1008 close. We give it up to 8s to land.
    let mut saw_policy_close = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let msg = tokio::time::timeout(Duration::from_secs(8), ws.next()).await;
        match msg {
            Ok(Some(Ok(Message::Close(Some(frame))))) => {
                assert_eq!(
                    frame.code,
                    CloseCode::Policy,
                    "expected WS close code 1008 (Policy), got {:?}",
                    frame.code
                );
                saw_policy_close = true;
                break;
            }
            Ok(Some(Ok(Message::Close(None)))) => {
                panic!("close received but with no frame; expected 1008 Policy");
            }
            Ok(Some(Ok(_other))) => continue,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        saw_policy_close,
        "server must close pending-auth client with 1008 within 8s"
    );

    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_auth_hello_can_be_retried_with_valid_token() {
    let (server, drain) = start_bridge(AuthConfig::with_token("s3cret")).await;
    let url = format!("ws://{}", server.local_addr);

    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    // First attempt: wrong token → AUTH_REJECTED.
    let bad = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "auth.hello",
        "params": { "token": "WRONG" },
        "id": 20
    });
    ws.send(Message::Text(bad.to_string().into()))
        .await
        .unwrap();
    let resp = await_response(&mut ws, 20).await;
    assert_eq!(
        resp["error"]["code"].as_i64().unwrap(),
        hypehouse_engine::bridge::AUTH_REJECTED as i64
    );

    // Second attempt: correct token. Server must still be PendingAuth +
    // accept the retry within the 5s window.
    let good = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "auth.hello",
        "params": { "token": "s3cret" },
        "id": 21
    });
    ws.send(Message::Text(good.to_string().into()))
        .await
        .unwrap();
    let resp = await_response(&mut ws, 21).await;
    assert_eq!(resp["result"]["authed"], serde_json::Value::Bool(true));

    // And submit_event now works.
    let evt = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "engine.submit_event",
        "params": { "kind": { "DeckPlay": { "deck": "A" } } },
        "id": 22
    });
    ws.send(Message::Text(evt.to_string().into()))
        .await
        .unwrap();
    let resp = await_response(&mut ws, 22).await;
    assert_eq!(resp["result"]["accepted"], serde_json::Value::Bool(true));
    let _ev = drain
        .recv_timeout(Duration::from_secs(2))
        .expect("forwarded event must arrive after auth");

    ws.close(None).await.ok();
    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_authenticated_client_skips_auth_hello() {
    // Native client path: presenting the Authorization header at the
    // handshake transitions to Authed immediately. The browser-only
    // auth.hello flow is opt-in, not required.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (server, drain) = start_bridge(AuthConfig::with_token("s3cret")).await;
    let url = format!("ws://{}", server.local_addr);

    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", "Bearer s3cret".parse().unwrap());
    let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();

    // No auth.hello — go straight to submit_event.
    let evt = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "engine.submit_event",
        "params": { "kind": { "DeckPlay": { "deck": "A" } } },
        "id": 30
    });
    ws.send(Message::Text(evt.to_string().into()))
        .await
        .unwrap();
    let resp = await_response(&mut ws, 30).await;
    assert!(
        resp.get("error").is_none() || resp["error"].is_null(),
        "{resp}"
    );
    assert_eq!(resp["result"]["accepted"], serde_json::Value::Bool(true));
    let _ev = drain
        .recv_timeout(Duration::from_secs(2))
        .expect("header-authed submit must reach the control loop");

    ws.close(None).await.ok();
    server.shutdown().await.unwrap();
}
