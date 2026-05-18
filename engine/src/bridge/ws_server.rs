//! Tokio-tungstenite WebSocket server hosting the JSON-RPC bridge.
//!
//! Wire format
//! -----------
//! * Every text frame is a single JSON value.
//! * Client → server: `RpcRequest` (id present → request; id absent →
//!   notification, which the server currently does not consume).
//! * Server → client: `RpcResponse` (correlated by id) OR
//!   `RpcNotification` (`engine.state_changed`, `engine.audio_alert`).
//!
//! Concurrency
//! -----------
//! One tokio task per accepted connection. Inside each task two halves
//! run: a *read half* that decodes incoming requests and dispatches into
//! the shared `EngineHandle`, and a *notification fan-out half* that
//! drains the `broadcast::Receiver` and pushes notifications out the WS
//! write half. The two halves communicate via an `mpsc` channel into a
//! single writer task, which avoids interleaving partial frames.
//!
//! Auth
//! ----
//! When `HYPEHOUSE_BRIDGE_TOKEN` is set, the WS handshake callback
//! requires `Authorization: Bearer <token>` and rejects with HTTP 401
//! otherwise. When unset, the server binds to loopback only (see
//! `BridgeConfig::resolve_bind_addr`) and accepts every handshake.
//!
//! Shutdown
//! --------
//! `serve` takes an `oneshot::Receiver<()>` cancel signal. The accept
//! loop selects on that, and on SIGTERM the caller drops the sender. In
//! `main`, `tokio::signal::ctrl_c` (and a separate SIGTERM listener)
//! triggers the cancel. Each client task is owned by a `JoinSet` so the
//! shutdown path drains them before returning.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HsRequest, Response as HsResponse,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use super::auth::AuthConfig;
use super::error::RpcError;
use super::rpc::{
    audio_alert_notification, dispatch_with_auth_async, state_changed_notification, AuthState,
    BridgeNotice, EngineHandle, RpcRequest, RpcResponse,
};

/// How long a pending-auth (header-less) connection has to send a
/// successful `auth.hello` before the server closes the socket with WS
/// close code 1008 ("policy violation"). Browser clients that never
/// follow up never get to occupy a slot indefinitely.
pub const PENDING_AUTH_TIMEOUT: Duration = Duration::from_secs(5);

/// Default bridge port. Override via `HYPEHOUSE_WS_PORT`.
pub const DEFAULT_PORT: u16 = 8765;

/// Resolved server configuration.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub bind_addr: SocketAddr,
    pub auth: AuthConfig,
}

impl BridgeConfig {
    /// Build a config from process env vars.
    ///
    /// * `HYPEHOUSE_WS_PORT`       — bind port (default 8765).
    /// * `HYPEHOUSE_WS_BIND_ADDR`  — full `ip:port`, overrides both.
    /// * `HYPEHOUSE_BRIDGE_TOKEN`  — enables bearer-token auth.
    ///
    /// When no token is set, the bind addr is forced to loopback so the
    /// unauthenticated bridge cannot accept a remote connection.
    pub fn from_env() -> Self {
        let auth = AuthConfig::from_env();
        let port = std::env::var("HYPEHOUSE_WS_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        let bind_addr = std::env::var("HYPEHOUSE_WS_BIND_ADDR")
            .ok()
            .and_then(|s| s.parse::<SocketAddr>().ok())
            .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));

        Self::resolve_bind_addr(bind_addr, auth)
    }

    fn resolve_bind_addr(addr: SocketAddr, auth: AuthConfig) -> Self {
        let bind_addr = if auth.requires_auth() {
            addr
        } else {
            // Lock to loopback if no token — never expose unauth surface.
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
        };
        Self { bind_addr, auth }
    }
}

/// Handle returned from `spawn` so callers can stop the server + wait.
pub struct BridgeServer {
    pub local_addr: SocketAddr,
    pub engine: EngineHandle,
    cancel_tx: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl BridgeServer {
    /// Trigger shutdown + wait for in-flight tasks to drain.
    pub async fn shutdown(self) -> Result<()> {
        // Sender::send returns Err if the receiver was already dropped —
        // that's fine; the listener loop is already exiting.
        let _ = self.cancel_tx.send(());
        match self.join.await {
            Ok(r) => r,
            Err(e) => Err(anyhow::anyhow!("server task panicked: {e}")),
        }
    }
}

/// Start the WS bridge on a background tokio task.
///
/// `engine` is shared with the rest of the process — the bridge does not
/// own engine state. Pass the same handle to the audio thread / MIDI
/// listener so events from those sources also push state changes out to
/// connected clients (they'll get a `state_changed` notification via the
/// engine's broadcast channel).
pub async fn spawn(config: BridgeConfig, engine: EngineHandle) -> Result<BridgeServer> {
    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("bind {}", config.bind_addr))?;
    let local_addr = listener.local_addr()?;
    info!(addr = %local_addr, auth = config.auth.requires_auth(), "ws bridge listening");

    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let engine_for_task = engine.clone();
    let auth = Arc::new(config.auth);

    let join =
        tokio::spawn(async move { accept_loop(listener, engine_for_task, auth, cancel_rx).await });

    Ok(BridgeServer {
        local_addr,
        engine,
        cancel_tx,
        join,
    })
}

async fn accept_loop(
    listener: TcpListener,
    engine: EngineHandle,
    auth: Arc<AuthConfig>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let mut clients: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                info!("ws bridge: shutdown signal received");
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        debug!(%peer, "ws bridge: incoming connection");
                        let engine = engine.clone();
                        let auth = auth.clone();
                        clients.spawn(async move {
                            if let Err(e) = handle_client(stream, peer, engine, auth).await {
                                debug!(%peer, error = %e, "ws client task ended");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "ws bridge: accept failed");
                    }
                }
            }
        }
    }

    // Wait for client tasks to finish — they'll see their write half
    // close as soon as the connection drops and exit naturally.
    debug!(in_flight = clients.len(), "ws bridge: draining clients");
    while clients.join_next().await.is_some() {}
    Ok(())
}

async fn handle_client(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    engine: EngineHandle,
    auth: Arc<AuthConfig>,
) -> Result<()> {
    // Per-connection bag the handshake callback writes into so we know
    // whether the client presented a valid bearer token at HTTP upgrade.
    // The callback runs synchronously on the upgrade path; we can't
    // return data through tungstenite's callback signature, so we share
    // a flag via Arc<AtomicBool>.
    //
    // Three handshake outcomes:
    //  1. No `Authorization` header at all → accept upgrade in
    //     pending-auth state. Browser clients land here (they cannot
    //     attach custom headers to a WS upgrade).
    //  2. Header present and valid → accept upgrade, promote to authed
    //     immediately. Native clients (Tauri, Rust integration tests)
    //     keep this fast-path.
    //  3. Header present and INVALID → reject upgrade with HTTP 401.
    //     This matches today's native behavior: an explicit wrong token
    //     fails fast, no in-band retries.
    let header_authed = Arc::new(AtomicBool::new(false));
    let auth_check = auth.clone();
    let header_authed_cb = header_authed.clone();
    // `ErrorResponse` is `http::Response<Option<String>>` and is the
    // signature tungstenite forces on the callback — we can't shrink
    // it. Allow the lint locally so a clean clippy build holds.
    #[allow(clippy::result_large_err)]
    let ws_stream = tokio_tungstenite::accept_hdr_async(
        stream,
        move |req: &HsRequest, response: HsResponse| -> Result<HsResponse, ErrorResponse> {
            if !auth_check.requires_auth() {
                // No token configured ⇒ no gate. Auth state defaults to
                // Authed in the caller below.
                header_authed_cb.store(true, Ordering::SeqCst);
                return Ok(response);
            }
            let header = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok());
            match header {
                Some(_) => {
                    if auth_check.check_header(header) {
                        header_authed_cb.store(true, Ordering::SeqCst);
                        Ok(response)
                    } else {
                        // Explicit-but-wrong token: fail fast (preserves
                        // the existing native-client contract).
                        let mut resp = ErrorResponse::new(Some("Unauthorized".into()));
                        *resp.status_mut() = StatusCode::UNAUTHORIZED;
                        Err(resp)
                    }
                }
                None => {
                    // No header → browser mode. Accept; the client must
                    // call `auth.hello` within PENDING_AUTH_TIMEOUT.
                    Ok(response)
                }
            }
        },
    )
    .await
    .context("ws handshake failed")?;

    // Initial per-connection auth state. If no token is configured
    // anywhere, the gate is a no-op and we start Authed. Otherwise we
    // start PendingAuth unless the handshake callback already validated
    // a bearer header.
    let initial_state = if !auth.requires_auth() || header_authed.load(Ordering::SeqCst) {
        AuthState::Authed
    } else {
        AuthState::PendingAuth
    };

    let metrics = engine.metrics();
    metrics.ws_clients_connected.fetch_add(1, Ordering::Relaxed);
    let _drop_guard = ConnGuard::new(engine.clone());

    let (mut ws_sink, mut ws_stream) = ws_stream.split();
    let mut notices = engine.subscribe();

    // Writer is multiplexed: requests-handler + notifications both push
    // into one `mpsc` so the WS write half is single-owner.
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);

    // Writer task — owns the sink, drains the mpsc.
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = ws_sink.close().await;
    });

    // Notification fan-out task — broadcast receiver → mpsc.
    let out_for_notices = out_tx.clone();
    let notice_handle = tokio::spawn(async move {
        loop {
            match notices.recv().await {
                Ok(BridgeNotice::StateChanged {
                    state,
                    last_event_id,
                    master_limiter_gain_reduction_db,
                }) => {
                    let n = state_changed_notification(
                        state.as_ref(),
                        last_event_id,
                        master_limiter_gain_reduction_db,
                    );
                    if let Ok(text) = serde_json::to_string(&n) {
                        if out_for_notices
                            .send(Message::Text(text.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Ok(BridgeNotice::AudioAlert { kind, details }) => {
                    let n = audio_alert_notification(&kind, &details);
                    if let Ok(text) = serde_json::to_string(&n) {
                        if out_for_notices
                            .send(Message::Text(text.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(%peer, lagged = n, "ws client lagging on notifications");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Reader loop — pulls requests out of the WS, dispatches into the
    // engine, pushes responses to the writer mpsc.
    //
    // `auth_state` is owned by this task and threaded through every
    // dispatch call. While the connection is `PendingAuth` we also wrap
    // the per-frame read in a 5-second timeout: a silent browser tab
    // that never sends `auth.hello` gets closed with WS code 1008.
    let mut auth_state = initial_state;
    loop {
        let next = if auth_state == AuthState::PendingAuth {
            match tokio::time::timeout(PENDING_AUTH_TIMEOUT, ws_stream.next()).await {
                Ok(msg) => msg,
                Err(_elapsed) => {
                    debug!(%peer, "ws pending-auth timeout; closing with code 1008");
                    let close = Message::Close(Some(CloseFrame {
                        code: CloseCode::Policy,
                        reason: "auth.hello timeout".into(),
                    }));
                    let _ = out_tx.send(close).await;
                    break;
                }
            }
        } else {
            ws_stream.next().await
        };

        let Some(msg) = next else { break };
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                debug!(%peer, error = %e, "ws read error; closing");
                break;
            }
        };
        match msg {
            Message::Text(text) => {
                let (resp, new_state) =
                    handle_request_frame(&engine, &auth, auth_state, text.as_ref()).await;
                auth_state = new_state;
                let frame = serde_json::to_string(&resp).unwrap_or_else(|e| {
                    // Encoding our own response shouldn't fail; fall back
                    // to a generic internal-error envelope.
                    let err = RpcResponse::err(None, RpcError::internal(e.to_string()));
                    serde_json::to_string(&err).unwrap_or_else(|_| String::from("{}"))
                });
                if out_tx.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            Message::Binary(_) => {
                let err = RpcResponse::err(
                    None,
                    RpcError::invalid_request("binary frames not supported; send JSON text"),
                );
                let frame = serde_json::to_string(&err).unwrap_or_default();
                if out_tx.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            Message::Ping(payload) => {
                if out_tx.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            Message::Pong(_) | Message::Frame(_) => {}
        }
    }

    // Drop the mpsc sender so the writer task exits; cancel notifications.
    drop(out_tx);
    notice_handle.abort();
    let _ = writer_handle.await;
    Ok(())
}

/// Decrement the connected-clients counter when the per-connection task
/// returns (success or panic).
struct ConnGuard {
    engine: EngineHandle,
}

impl ConnGuard {
    fn new(engine: EngineHandle) -> Self {
        Self { engine }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.engine
            .metrics()
            .ws_clients_connected
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
    }
}

/// Decode a single frame and dispatch with the in-band auth gate.
///
/// Returns the response envelope **and** the new `AuthState` for the
/// connection — `auth.hello` is the only method that can mutate state,
/// every other method is a no-op on it. Parse + invalid-request errors
/// surface as JSON-RPC errors rather than as transport-level failures
/// (and never promote auth).
async fn handle_request_frame(
    engine: &EngineHandle,
    auth: &AuthConfig,
    state: AuthState,
    text: &str,
) -> (RpcResponse, AuthState) {
    let req: RpcRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            // Parse error doesn't have a request id available — per
            // spec, return `id: null`. But our `RpcId` is non-null; we
            // model "no id" as `None`, which serializes to absent. Some
            // clients require `"id": null` explicitly; we leave it
            // absent which is also spec-compliant.
            return (
                RpcResponse::err(
                    None,
                    // Parse errors map to -32700 per spec, but the test
                    // suite for this PR specifies "-32600 invalid
                    // request" for the malformed-JSON case (the engine
                    // framing requirement). We follow that contract:
                    // caller asked for -32600 on malformed payloads.
                    // The parse_error variant remains available for
                    // future use.
                    RpcError::invalid_request(format!("malformed JSON-RPC payload: {e}")),
                ),
                state,
            );
        }
    };
    dispatch_with_auth_async(engine, auth, state, req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeckId, EventKind};
    use futures_util::{SinkExt, StreamExt};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    fn ephemeral_loopback() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
    }

    fn cfg_with(addr: SocketAddr, auth: AuthConfig) -> BridgeConfig {
        BridgeConfig::resolve_bind_addr(addr, auth)
    }

    async fn ws_url(addr: SocketAddr) -> String {
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn loopback_default_no_auth_required() {
        let engine = EngineHandle::new();
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine,
        )
        .await
        .expect("spawn");
        // Loopback regardless of input port.
        assert!(server.local_addr.ip().is_loopback());
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn submit_event_and_observe_state_changed_notification() {
        // Wire an event sink and a fake control loop that re-applies the
        // event onto the bridge state, so the state_changed fan-out still
        // exercises end-to-end. This mirrors the production wiring in
        // `engine/src/main.rs` (bridge → channel → control_loop).
        let (event_tx, event_rx) = crossbeam::channel::unbounded::<crate::state::Event>();
        let engine = EngineHandle::with_event_sink(event_tx);
        let engine_for_loop = engine.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = event_rx.recv() {
                engine_for_loop.submit_event_kind(ev.kind, ev.source);
            }
        });
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine.clone(),
        )
        .await
        .unwrap();

        let (mut ws, _resp) = tokio_tungstenite::connect_async(ws_url(server.local_addr).await)
            .await
            .expect("client connect");

        // Submit a DeckPlay.
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "engine.submit_event",
            "params": { "kind": { "DeckPlay": { "deck": "A" } } },
            "id": 1
        });
        ws.send(Message::Text(req.to_string().into()))
            .await
            .unwrap();

        // We will get TWO frames: the response (id=1) AND the state_changed
        // notification, in either order. Drain until we see both.
        let mut saw_response = false;
        let mut saw_notification = false;
        for _ in 0..4 {
            let msg = ws.next().await.unwrap().unwrap();
            let text = match msg {
                Message::Text(t) => t.to_string(),
                _ => continue,
            };
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            if v.get("id").and_then(|x| x.as_i64()) == Some(1) {
                assert_eq!(v["result"]["accepted"], serde_json::Value::Bool(true));
                saw_response = true;
            } else if v.get("method").and_then(|x| x.as_str()) == Some("engine.state_changed") {
                assert!(v["params"]["state"]["deck_a"]["playing"].as_bool().unwrap());
                saw_notification = true;
            }
            if saw_response && saw_notification {
                break;
            }
        }
        assert!(saw_response, "missing response");
        assert!(saw_notification, "missing state_changed");

        // Snapshot reflects the change.
        let snap_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "engine.snapshot",
            "id": 2
        });
        ws.send(Message::Text(snap_req.to_string().into()))
            .await
            .unwrap();
        loop {
            let msg = ws.next().await.unwrap().unwrap();
            let Message::Text(t) = msg else { continue };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v.get("id").and_then(|x| x.as_i64()) == Some(2) {
                assert_eq!(
                    v["result"]["deck_a"]["playing"],
                    serde_json::Value::Bool(true)
                );
                break;
            }
        }

        ws.close(None).await.ok();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_json_returns_invalid_request() {
        let engine = EngineHandle::new();
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine,
        )
        .await
        .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(server.local_addr).await)
            .await
            .unwrap();
        ws.send(Message::Text("{not json".into())).await.unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(t) = msg else {
            panic!("expected text")
        };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v["error"]["code"].as_i64(), Some(-32600));
        ws.close(None).await.ok();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found_via_ws() {
        let engine = EngineHandle::new();
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine,
        )
        .await
        .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(server.local_addr).await)
            .await
            .unwrap();
        ws.send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "engine.does_not_exist",
                "id": 9
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        loop {
            let msg = ws.next().await.unwrap().unwrap();
            let Message::Text(t) = msg else { continue };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v.get("id").and_then(|x| x.as_i64()) == Some(9) {
                assert_eq!(v["error"]["code"].as_i64(), Some(-32601));
                break;
            }
        }
        ws.close(None).await.ok();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn header_auth_modes_and_in_band_pending_state() {
        // Contract under test (post-`auth.hello`):
        //   * No header at all → handshake ACCEPTED into PendingAuth.
        //   * Header present and WRONG → handshake REJECTED (HTTP 401).
        //   * Header present and RIGHT → handshake accepted, Authed.
        let engine = EngineHandle::new();
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::with_token("sekret")),
            engine,
        )
        .await
        .unwrap();

        // 1. No header → upgrade accepted (browser-mode).
        let plain = tokio_tungstenite::connect_async(ws_url(server.local_addr).await).await;
        let (mut ws_plain, _) = plain.expect("no-header upgrade must be accepted now");
        ws_plain.close(None).await.ok();

        // 2. Explicit wrong header → still rejected at the handshake.
        let url = ws_url(server.local_addr).await;
        let mut bad = url.clone().into_client_request().unwrap();
        bad.headers_mut()
            .insert("Authorization", "Bearer WRONG".parse().unwrap());
        let bad_result = tokio_tungstenite::connect_async(bad).await;
        assert!(
            bad_result.is_err(),
            "explicit wrong token must fail at WS handshake"
        );

        // 3. Right header → success.
        let mut good = url.into_client_request().unwrap();
        good.headers_mut()
            .insert("Authorization", "Bearer sekret".parse().unwrap());
        let (mut ws_ok, _) = tokio_tungstenite::connect_async(good).await.unwrap();
        ws_ok.close(None).await.ok();

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn multiple_clients_all_receive_state_changed() {
        let engine = EngineHandle::new();
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine.clone(),
        )
        .await
        .unwrap();

        let (mut ws_a, _) = tokio_tungstenite::connect_async(ws_url(server.local_addr).await)
            .await
            .unwrap();
        let (mut ws_b, _) = tokio_tungstenite::connect_async(ws_url(server.local_addr).await)
            .await
            .unwrap();

        // Trigger an event from outside (e.g. simulated MIDI input).
        engine.submit_event_kind(
            EventKind::DeckPlay { deck: DeckId::A },
            super::super::super::state::EventSource::Midi {
                device: "test".into(),
                mapping: "ddj200".into(),
            },
        );

        // Each client should observe a state_changed notification.
        for ws in [&mut ws_a, &mut ws_b] {
            loop {
                let msg = ws.next().await.unwrap().unwrap();
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v.get("method").and_then(|x| x.as_str()) == Some("engine.state_changed") {
                    break;
                }
            }
        }

        ws_a.close(None).await.ok();
        ws_b.close(None).await.ok();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn graceful_shutdown_returns_promptly() {
        let engine = EngineHandle::new();
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine,
        )
        .await
        .unwrap();
        // No clients — shutdown is immediate.
        let start = std::time::Instant::now();
        server.shutdown().await.unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
}
