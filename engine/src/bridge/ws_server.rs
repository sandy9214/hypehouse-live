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
use super::ratelimit::{Decision as RateLimitDecision, RateLimiter};
use super::rpc::{
    audio_alert_notification, decode_error_notification, dispatch_with_auth_async, method,
    state_changed_notification, AuthState, BridgeNotice, EngineHandle, RpcRequest, RpcResponse,
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
                    sidechain_gain_reduction_db,
                    clock_source,
                    perf,
                }) => {
                    let n = state_changed_notification(
                        state.as_ref(),
                        last_event_id,
                        master_limiter_gain_reduction_db,
                        sidechain_gain_reduction_db,
                        clock_source,
                        perf,
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
                Ok(BridgeNotice::DecodeError {
                    deck,
                    track_id,
                    category,
                    error,
                }) => {
                    let n = decode_error_notification(deck, &track_id, &category, &error);
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
    //
    // `rate_limiter` is a per-connection token bucket guarding
    // `engine.submit_event` (200 events/sec sustained, 1000 burst). A
    // malicious/buggy UI that spams submit_event 10 000/sec is capped
    // so legitimate MIDI/UI events still reach the bounded control-
    // loop channel. The env override `HYPEHOUSE_RATE_LIMIT_DISABLED=1`
    // turns the gate into a no-op for dev/test.
    let mut auth_state = initial_state;
    let mut rate_limiter = RateLimiter::new();
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
                let (resp, new_state) = handle_request_frame(
                    &engine,
                    &auth,
                    auth_state,
                    &mut rate_limiter,
                    text.as_ref(),
                )
                .await;
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

/// Decode a single frame and dispatch with the in-band auth gate +
/// per-connection rate limiter.
///
/// Returns the response envelope **and** the new `AuthState` for the
/// connection — `auth.hello` is the only method that can mutate state,
/// every other method is a no-op on it. Parse + invalid-request errors
/// surface as JSON-RPC errors rather than as transport-level failures
/// (and never promote auth).
///
/// Rate-limit gate: when the decoded method is `engine.submit_event`
/// AND the connection is already `Authed`, one token is consumed from
/// the per-connection bucket. Exhaustion returns `-32003 RATE_LIMITED`
/// with a `{ retry_after_ms }` data payload and short-circuits before
/// the request hits the dispatcher. Other methods (`auth.hello`,
/// `engine.snapshot`, etc.) bypass the limiter. The token is consumed
/// BEFORE dispatch so a flood can't drain the control-loop channel
/// even transiently.
async fn handle_request_frame(
    engine: &EngineHandle,
    auth: &AuthConfig,
    state: AuthState,
    rate_limiter: &mut RateLimiter,
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
    if state == AuthState::Authed && req.method == method::SUBMIT_EVENT {
        if let RateLimitDecision::Deny { retry_after_ms } = rate_limiter.try_acquire() {
            return (
                RpcResponse::err(req.id, RpcError::rate_limited(retry_after_ms)),
                state,
            );
        }
    }
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
    async fn rate_limit_burst_then_denies_with_retry_after_ms() {
        // Flood `engine.submit_event` faster than the 200/sec refill
        // and verify the per-connection token bucket eventually emits
        // `-32003 RATE_LIMITED` with a structured `retry_after_ms`.
        // Burst is 1 000; we send 2 500 frames so even with refill
        // during the back-to-back send loop (~1 token every 5 ms of
        // wall-clock slop on a slow CI host) the bucket cannot help
        // but drain.
        //
        // Wire a sink we drain immediately so the control-loop channel
        // never becomes the bottleneck — the limiter is the only thing
        // we want rejecting frames. The drained sink also keeps the
        // engine in "real" submit_event mode (not the `-32001` path).
        let (event_tx, event_rx) = crossbeam::channel::unbounded::<crate::state::Event>();
        std::thread::spawn(move || while event_rx.recv().is_ok() {});
        let engine = EngineHandle::with_event_sink(event_tx);
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine.clone(),
        )
        .await
        .unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(server.local_addr).await)
            .await
            .unwrap();

        let mk_submit = |id: i64| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "engine.submit_event",
                "params": { "kind": { "DeckPlay": { "deck": "A" } } },
                "id": id,
            })
            .to_string()
        };

        const FLOOD: i64 = 2_500;
        for id in 0..FLOOD {
            ws.send(Message::Text(mk_submit(id).into())).await.unwrap();
        }

        let mut accepted = 0u32;
        let mut denied = 0u32;
        let mut first_denied_retry_after: Option<u64> = None;
        let mut seen = 0u32;
        // Each `submit_event` produces exactly one response frame (plus
        // optionally a state_changed broadcast we filter out below).
        while seen < FLOOD as u32 {
            let msg = ws.next().await.unwrap().unwrap();
            let Message::Text(t) = msg else { continue };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v.get("id").and_then(|x| x.as_i64()).is_none() {
                continue; // state_changed notification — skip
            }
            seen += 1;
            if let Some(code) = v["error"]["code"].as_i64() {
                assert_eq!(code, -32003, "non-rate-limit error: {v}");
                if first_denied_retry_after.is_none() {
                    first_denied_retry_after = v["error"]["data"]["retry_after_ms"].as_u64();
                }
                denied += 1;
            } else {
                assert_eq!(v["result"]["accepted"], serde_json::Value::Bool(true));
                accepted += 1;
            }
        }

        // Burst capacity is 1 000, so accepted must be ≥ 1 000 (and
        // can legitimately exceed it because refill kicks in during
        // the send loop). Denied must be > 0 — otherwise the limiter
        // didn't trigger at all.
        assert!(
            accepted >= 1_000,
            "burst should accept at least the full capacity (1 000), got {accepted}",
        );
        assert!(
            denied > 0,
            "flooding {FLOOD} events must trigger ≥1 RATE_LIMITED, got 0",
        );
        let retry = first_denied_retry_after.expect("denied frame must carry retry_after_ms");
        assert!(retry >= 1, "retry_after_ms must be ≥ 1, got {retry}");
        assert!(
            retry <= 5_000,
            "retry_after_ms suspiciously large: {retry} (expected ≤ 5 000 ms)",
        );

        ws.close(None).await.ok();
        server.shutdown().await.unwrap();
    }

    /// Test helper — flood a WS connection with `engine.submit_event`
    /// frames until the server returns at least one `-32003
    /// RATE_LIMITED`. Returns the count of accepted frames before the
    /// first deny. Fails the test if `max_frames` is exhausted without
    /// triggering the limiter (defensive — keeps the test bounded).
    async fn flood_until_rate_limited(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        max_frames: u32,
    ) -> u32 {
        // Batch the sends, then drain responses. We keep both halves
        // interleaved to avoid filling the tokio mpsc on the server.
        let mut accepted_before_deny = 0u32;
        for (next_id, _) in (100_000_i64..).zip(0..max_frames) {
            let id = next_id;
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "engine.submit_event",
                "params": { "kind": { "DeckPlay": { "deck": "A" } } },
                "id": id,
            })
            .to_string();
            ws.send(Message::Text(body.into())).await.unwrap();
            loop {
                let msg = ws.next().await.unwrap().unwrap();
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v.get("id").and_then(|x| x.as_i64()) != Some(id) {
                    continue; // state_changed or older response
                }
                if v["error"]["code"].as_i64() == Some(-32003) {
                    return accepted_before_deny;
                }
                assert!(
                    v.get("error").is_none(),
                    "unexpected non-rate-limit error: {v}"
                );
                accepted_before_deny += 1;
                break;
            }
        }
        panic!("limiter never tripped after {max_frames} frames (accepted {accepted_before_deny})");
    }

    #[tokio::test]
    async fn rate_limit_regenerates_after_short_wait() {
        // Drain the bucket via the helper, then wait long enough for
        // many tokens to refill (250 ms at 200/sec = ~50 tokens) and
        // verify a fresh submit_event succeeds.
        let (event_tx, event_rx) = crossbeam::channel::unbounded::<crate::state::Event>();
        std::thread::spawn(move || while event_rx.recv().is_ok() {});
        let engine = EngineHandle::with_event_sink(event_tx);
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine.clone(),
        )
        .await
        .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(server.local_addr).await)
            .await
            .unwrap();

        // Drain the bucket — we don't care about the exact accepted
        // count, only that the limiter eventually denied.
        let _ = flood_until_rate_limited(&mut ws, 3_000).await;

        // Sleep 250 ms — at 200/sec that's ~50 tokens regenerated,
        // well above any single-token-timing flake.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        ws.send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "engine.submit_event",
                "params": { "kind": { "DeckPlay": { "deck": "A" } } },
                "id": 3_000,
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
            if v.get("id").and_then(|x| x.as_i64()) == Some(3_000) {
                assert!(
                    v.get("error").is_none(),
                    "post-wait submit unexpectedly denied: {v}",
                );
                assert_eq!(v["result"]["accepted"], serde_json::Value::Bool(true));
                break;
            }
        }

        ws.close(None).await.ok();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rate_limit_does_not_apply_to_other_rpc_methods() {
        // Even with the bucket fully drained, `engine.snapshot`,
        // `engine.health`, and `engine.list_sessions` must continue to
        // succeed. We only rate-limit `engine.submit_event`.
        let (event_tx, event_rx) = crossbeam::channel::unbounded::<crate::state::Event>();
        std::thread::spawn(move || while event_rx.recv().is_ok() {});
        let engine = EngineHandle::with_event_sink(event_tx);
        let server = spawn(
            cfg_with(ephemeral_loopback(), AuthConfig::default()),
            engine.clone(),
        )
        .await
        .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(server.local_addr).await)
            .await
            .unwrap();

        // Drain the bucket so the bridge starts denying submit_event.
        let _ = flood_until_rate_limited(&mut ws, 3_000).await;

        // engine.snapshot — must succeed even when bucket is empty.
        ws.send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "engine.snapshot",
                "id": 6_001,
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
            if v.get("id").and_then(|x| x.as_i64()) == Some(6_001) {
                assert!(v.get("error").is_none(), "snapshot wrongly rate-limited");
                assert!(v["result"].is_object());
                break;
            }
        }

        // engine.health — must also succeed.
        ws.send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "engine.health",
                "id": 6_002,
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
            if v.get("id").and_then(|x| x.as_i64()) == Some(6_002) {
                assert!(v.get("error").is_none(), "health wrongly rate-limited");
                break;
            }
        }

        // engine.list_sessions — also unaffected.
        ws.send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "engine.list_sessions",
                "id": 6_003,
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
            if v.get("id").and_then(|x| x.as_i64()) == Some(6_003) {
                // The session list may legitimately fail with an
                // internal error in CI sandboxes without a writable
                // sessions dir — but it must NOT be `-32003`.
                let code = v["error"]["code"].as_i64();
                assert_ne!(
                    code,
                    Some(-32003),
                    "list_sessions wrongly rate-limited: {v}"
                );
                break;
            }
        }

        ws.close(None).await.ok();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rate_limit_per_connection_isolated_between_clients() {
        // The limiter is per-connection: client A draining its bucket
        // must NOT affect client B's quota.
        let (event_tx, event_rx) = crossbeam::channel::unbounded::<crate::state::Event>();
        std::thread::spawn(move || while event_rx.recv().is_ok() {});
        let engine = EngineHandle::with_event_sink(event_tx);
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

        // Drain client A (the helper flushes responses inline so the
        // server's mpsc never backs up).
        let _ = flood_until_rate_limited(&mut ws_a, 3_000).await;

        // Client B's bucket is still full — its first submit_event
        // must succeed.
        ws_b.send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "engine.submit_event",
                "params": { "kind": { "DeckPlay": { "deck": "A" } } },
                "id": 8_000,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        loop {
            let msg = ws_b.next().await.unwrap().unwrap();
            let Message::Text(t) = msg else { continue };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v.get("id").and_then(|x| x.as_i64()) == Some(8_000) {
                assert!(
                    v.get("error").is_none(),
                    "client B wrongly rate-limited by client A's flood: {v}"
                );
                break;
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
