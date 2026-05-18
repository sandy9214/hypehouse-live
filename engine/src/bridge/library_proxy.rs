//! HTTP-RPC proxy for the `library.*` JSON-RPC namespace.
//!
//! Why this exists
//! ---------------
//! For the Tauri desktop shell we want exactly **one** WebSocket open from
//! the UI to a local service (the engine bridge on `127.0.0.1:8765`).
//! The track-library catalog lives in the Python copilot process (it owns
//! the analyzer + SQLite store — see `copilot/library_rpc.py`), so the
//! engine bridge proxies the UI's `library.*` calls to the copilot via a
//! tiny JSON-RPC-over-HTTP hop. The copilot exposes the same handlers it
//! does on its own WS surface; the proxy just speaks HTTP POST.
//!
//! Why HTTP, not WebSocket
//! -----------------------
//! The proxy is a fan-in/fan-out for short-lived request/response pairs.
//! HTTP gives us:
//!
//! * One in-flight call ⇄ one TCP connection (no shared multiplexer
//!   state — the proxy is stateless past the `reqwest::Client` pool).
//! * Native per-request timeout via `reqwest::ClientBuilder::timeout`.
//! * Trivial mockability for tests (any HTTP harness works).
//!
//! A WS client would have to hold connection state, correlate ids, and
//! re-open on copilot restarts — none of which buys anything here.
//!
//! Wire shape
//! ----------
//! The proxy posts a standard JSON-RPC 2.0 request body:
//!
//! ```json
//! { "jsonrpc": "2.0", "id": 1, "method": "library.list_tracks",
//!   "params": { "limit": 100, "offset": 0 } }
//! ```
//!
//! and expects a standard JSON-RPC 2.0 response back:
//!
//! ```json
//! { "jsonrpc": "2.0", "id": 1, "result": { ... } }
//! ```
//!
//! or
//!
//! ```json
//! { "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "..." } }
//! ```
//!
//! Errors
//! ------
//! * `-32000 engine offline` — copilot unreachable (refused / DNS / timeout
//!   / TLS / read error). Returned with `data` carrying the underlying
//!   `reqwest` error message for diagnostics. We deliberately re-use the
//!   existing `ENGINE_OFFLINE` code because the UI already maps it to
//!   a "backend unavailable" banner; introducing a new code would force
//!   every existing consumer to fork its error path. The `data` payload
//!   discriminates "engine vs copilot" if a caller cares.
//! * `-32000 copilot proxy disabled` — `HYPEHOUSE_COPILOT_URL` is set to
//!   the empty string. Deployers use this to hard-disable the proxy when
//!   running the engine without a copilot (degraded mode).
//! * Copilot-returned errors are passed through verbatim — the proxy
//!   surfaces the upstream `code` + `message` + `data` so the UI sees the
//!   same envelope it would see talking to copilot directly.

use std::env;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::RpcError;
use super::rpc::JSONRPC_VERSION;

/// Env var that overrides the default copilot RPC URL.
///
/// * Unset ⇒ default `http://127.0.0.1:8766/rpc`.
/// * Set to the empty string ⇒ proxy disabled; every `library.*` call
///   returns `-32000 copilot proxy disabled`. Lets deployers run the
///   engine without a copilot without forking the dispatch path.
pub const ENV_COPILOT_URL: &str = "HYPEHOUSE_COPILOT_URL";

/// Default URL the proxy posts to when `HYPEHOUSE_COPILOT_URL` is unset.
pub const DEFAULT_COPILOT_URL: &str = "http://127.0.0.1:8766/rpc";

/// Per-call timeout. 5s is generous for in-process JSON-RPC but tight
/// enough that a hung copilot doesn't block the UI's library panel.
pub const COPILOT_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Process-wide `reqwest` client. Reused across calls so connection
/// pooling holds and we don't pay TCP+TLS setup on every request.
fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(COPILOT_RPC_TIMEOUT)
            // The copilot bind is loopback by default — no proxy chain
            // applies. Disabling system proxies avoids the macOS keychain
            // lookup that some HTTP libraries trigger on first call.
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Resolve the copilot RPC URL from env, with the proxy-disabled sentinel.
///
/// Returns:
/// * `Ok(Some(url))` — proxy enabled; URL is what callers should POST to.
/// * `Ok(None)` — proxy disabled (env var explicitly set to empty).
/// * `Ok(Some(DEFAULT_COPILOT_URL))` — env var unset; default applies.
pub fn resolve_copilot_url() -> Option<String> {
    match env::var(ENV_COPILOT_URL) {
        Ok(v) if v.is_empty() => None,
        Ok(v) => Some(v),
        Err(_) => Some(DEFAULT_COPILOT_URL.to_string()),
    }
}

/// JSON-RPC 2.0 request body posted to the copilot.
///
/// The `id` is fixed to `1` because each HTTP call is its own
/// request/response pair — the copilot doesn't pipeline frames over a
/// single transport so id collisions are impossible.
#[derive(Serialize)]
struct ProxiedRequest<'a> {
    jsonrpc: &'a str,
    method: &'a str,
    params: &'a Value,
    id: u32,
}

/// JSON-RPC 2.0 response shape returned by the copilot.
///
/// Both `result` and `error` are optional; exactly one is expected per the
/// spec. We surface either path verbatim to the original caller.
#[derive(Deserialize)]
struct ProxiedResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

/// Public surface — call `library.<method>` against the configured copilot.
///
/// Forwards `params` as the JSON-RPC body. Returns the unwrapped `result`
/// value on success, or a populated `RpcError` envelope on any failure.
/// Never panics; every network / parse failure is folded into
/// `RpcError::engine_offline` with the underlying reason in `data` so the
/// caller can log it.
pub async fn forward_library_call(method: &str, params: Value) -> Result<Value, RpcError> {
    let url = match resolve_copilot_url() {
        Some(u) => u,
        None => {
            return Err(RpcError::engine_offline("copilot proxy disabled"));
        }
    };

    let body = ProxiedRequest {
        jsonrpc: JSONRPC_VERSION,
        method,
        params: &params,
        id: 1,
    };

    let client = shared_client();
    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| copilot_unavailable(&url, &e))?;

    // Surface HTTP-layer failures (4xx/5xx) as engine_offline. The
    // copilot endpoint should always return 200 + a JSON-RPC envelope —
    // anything else is a service-level outage from the UI's POV.
    if !response.status().is_success() {
        let status = response.status();
        return Err(RpcError::engine_offline(format!(
            "copilot returned HTTP {status}"
        )));
    }

    let envelope: ProxiedResponse = response
        .json()
        .await
        .map_err(|e| RpcError::engine_offline(format!("copilot returned invalid JSON-RPC: {e}")))?;

    // Per JSON-RPC 2.0, exactly one of `result` / `error` is present.
    // We accept either; if both are absent we treat that as a malformed
    // response (also engine_offline-class because the caller didn't do
    // anything wrong).
    if let Some(err) = envelope.error {
        return Err(err);
    }
    if let Some(result) = envelope.result {
        return Ok(result);
    }
    Err(RpcError::engine_offline(
        "copilot returned envelope with neither result nor error",
    ))
}

/// Helper — turn a reqwest error into a structured `-32000 engine offline`
/// payload. Splits "the connection was refused" from "the connection
/// timed out" in the `data` field so logs are useful when a copilot is
/// degraded vs. completely missing.
fn copilot_unavailable(url: &str, err: &reqwest::Error) -> RpcError {
    let kind = if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect_refused"
    } else if err.is_request() {
        "request_failed"
    } else {
        "io_error"
    };
    RpcError::engine_offline(format!("copilot unavailable ({kind}) at {url}: {err}"))
}

/// Test-only env lock for `HYPEHOUSE_COPILOT_URL`.
///
/// Exposed publicly (gated on `cfg(test)`) so the rpc-dispatch tests in
/// `rpc.rs` share the same `Mutex` — both modules mutate the env var and
/// would race otherwise. We use a `std::sync::Mutex` because tests need
/// to acquire the gate from both sync (`#[test]`) and async
/// (`#[tokio::test]`) bodies; the std mutex is the only one that works
/// for both. The async tests carry an `#[allow(clippy::await_holding_lock)]`
/// — under the test runtime they execute on a `current_thread` flavour
/// so there's no real deadlock window.
#[cfg(test)]
pub fn copilot_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
// The shared env lock is a `std::sync::Mutex` so both sync `#[test]`
// and async `#[tokio::test]` bodies can acquire it. The async tests
// hold the guard across `.await`s under the default `current_thread`
// test runtime, so the lint's deadlock scenario doesn't apply.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;

    /// Tests use the shared lock so they don't race with the
    /// dispatch-side tests in `rpc.rs::tests`.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        super::copilot_env_lock()
    }

    fn clear_env() {
        std::env::remove_var(ENV_COPILOT_URL);
    }

    #[test]
    fn resolve_copilot_url_defaults_when_unset() {
        let _g = env_lock();
        clear_env();
        assert_eq!(resolve_copilot_url().as_deref(), Some(DEFAULT_COPILOT_URL));
    }

    #[test]
    fn resolve_copilot_url_returns_none_when_empty() {
        let _g = env_lock();
        std::env::set_var(ENV_COPILOT_URL, "");
        assert!(resolve_copilot_url().is_none());
        clear_env();
    }

    #[test]
    fn resolve_copilot_url_honors_override() {
        let _g = env_lock();
        std::env::set_var(ENV_COPILOT_URL, "http://example.invalid:1234/rpc");
        assert_eq!(
            resolve_copilot_url().as_deref(),
            Some("http://example.invalid:1234/rpc")
        );
        clear_env();
    }

    #[tokio::test]
    async fn forward_library_call_returns_disabled_when_env_empty() {
        let _g = env_lock();
        std::env::set_var(ENV_COPILOT_URL, "");
        let err = forward_library_call("library.list_tracks", serde_json::json!({}))
            .await
            .expect_err("disabled proxy must fail");
        clear_env();
        assert_eq!(err.code, super::super::error::ENGINE_OFFLINE);
        assert!(
            err.message.contains("engine offline"),
            "msg = {}",
            err.message
        );
        let data = err.data.as_ref().and_then(Value::as_str).unwrap_or("");
        assert!(
            data.contains("disabled"),
            "expected 'disabled' in data: {data}"
        );
    }

    /// Mock copilot — listens on an ephemeral port, echoes a configurable
    /// JSON-RPC response. Lightweight enough to spawn per-test without
    /// pulling a full HTTP framework.
    ///
    /// Reads request bytes until we've seen the complete request (headers
    /// plus Content-Length-sized body), then writes the canned JSON
    /// response with `Connection: close`. Sufficient for
    /// `reqwest::Client::post` which sends a single request and waits
    /// for the response.
    async fn spawn_mock_copilot(
        respond_with: serde_json::Value,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = serde_json::to_string(&respond_with).unwrap();
        let handle = tokio::spawn(async move {
            // Single-request server; the test makes exactly one call.
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 8192];
                let mut accumulated = Vec::new();
                let mut content_length: Option<usize> = None;
                let mut header_end: Option<usize> = None;
                // Read enough to satisfy the request — read until the
                // headers are complete AND the body length advertised by
                // Content-Length has arrived. Cap to keep the mock from
                // hanging on a misbehaving client.
                let read_deadline = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < read_deadline {
                    let read_fut = sock.read(&mut buf);
                    let n = match tokio::time::timeout(Duration::from_millis(200), read_fut).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) => break,
                        // No more bytes ready right now — but if we've
                        // got a complete request already, stop waiting.
                        Err(_) => {
                            if let (Some(end), Some(len)) = (header_end, content_length) {
                                if accumulated.len() >= end + len {
                                    break;
                                }
                            }
                            continue;
                        }
                    };
                    accumulated.extend_from_slice(&buf[..n]);
                    if header_end.is_none() {
                        if let Some(idx) = find_double_crlf(&accumulated) {
                            header_end = Some(idx + 4);
                            content_length = parse_content_length(&accumulated[..idx]);
                        }
                    }
                    if let (Some(end), Some(len)) = (header_end, content_length) {
                        if accumulated.len() >= end + len {
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, handle)
    }

    fn find_double_crlf(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    fn parse_content_length(headers: &[u8]) -> Option<usize> {
        let text = std::str::from_utf8(headers).ok()?;
        for line in text.lines() {
            let mut parts = line.splitn(2, ':');
            let name = parts.next()?.trim();
            let value = parts.next()?.trim();
            if name.eq_ignore_ascii_case("content-length") {
                return value.parse().ok();
            }
        }
        None
    }

    #[tokio::test]
    async fn library_list_tracks_forwards_to_copilot() {
        let _g = env_lock();
        let mock_result = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tracks": [
                    {
                        "id": "alpha",
                        "path": "/music/alpha.mp3",
                        "bpm": 124.0,
                        "camelot_key": "8B",
                        "energy": 0.5,
                        "duration_s": 200.0,
                        "beat_grid_anchor_ms": 0,
                        "beat_period_ms": 483.87,
                        "downbeats_ms": [],
                        "hot_cues": [null, null, null, null, null, null, null, null]
                    }
                ],
                "total": 1,
                "limit": 100,
                "offset": 0
            }
        });
        let (addr, handle) = spawn_mock_copilot(mock_result).await;
        std::env::set_var(ENV_COPILOT_URL, format!("http://{addr}/rpc"));
        let result = forward_library_call("library.list_tracks", serde_json::json!({"limit": 100}))
            .await
            .expect("mock copilot should respond");
        clear_env();
        let _ = handle.await;
        assert_eq!(result["total"], serde_json::json!(1));
        let tracks = result["tracks"].as_array().expect("tracks array");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0]["id"], serde_json::json!("alpha"));
    }

    #[tokio::test]
    async fn library_unreachable_returns_engine_offline() {
        let _g = env_lock();
        // Pick an unused loopback port (bind+drop) so we know nothing is
        // listening. The race window between drop and the proxy call is
        // tiny and the OS won't reuse the port that quickly.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        std::env::set_var(ENV_COPILOT_URL, format!("http://{addr}/rpc"));
        let err = forward_library_call("library.list_tracks", serde_json::json!({}))
            .await
            .expect_err("unreachable copilot must error");
        clear_env();
        assert_eq!(err.code, super::super::error::ENGINE_OFFLINE);
    }

    #[tokio::test]
    async fn library_hung_server_times_out() {
        let _g = env_lock();
        // Spawn a listener that accepts the TCP connection but never
        // writes a response. The proxy should hit COPILOT_RPC_TIMEOUT
        // and return engine_offline with a timeout marker.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                // Hold the socket open without writing.
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(sock);
            }
        });
        std::env::set_var(ENV_COPILOT_URL, format!("http://{addr}/rpc"));
        // Use a tighter override for the test by issuing a single call —
        // the 5s default keeps the test under 6s wall-clock. Faster
        // failure would require parameterising the timeout; we keep the
        // production default and accept the wait so the behaviour is
        // tested as deployed.
        let start = std::time::Instant::now();
        let err = forward_library_call("library.list_tracks", serde_json::json!({}))
            .await
            .expect_err("timeout must error");
        let elapsed = start.elapsed();
        clear_env();
        server_handle.abort();
        assert_eq!(err.code, super::super::error::ENGINE_OFFLINE);
        // Sanity-check that we actually waited for the timeout to fire
        // (~5s) rather than failing instantly. The bound is loose to
        // tolerate CI scheduler jitter.
        assert!(
            elapsed >= Duration::from_secs(4),
            "timeout fired too fast: {elapsed:?}"
        );
        let data = err.data.as_ref().and_then(Value::as_str).unwrap_or("");
        assert!(
            data.contains("timeout") || data.contains("unavailable"),
            "expected timeout marker in data: {data}"
        );
    }

    #[tokio::test]
    async fn library_upstream_error_passthrough() {
        let _g = env_lock();
        // Mock copilot returns a JSON-RPC error envelope; the proxy
        // must surface the upstream code+message verbatim, not wrap
        // them in -32000.
        let mock_err = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32602,
                "message": "Invalid params",
                "data": "track_id must be non-empty"
            }
        });
        let (addr, handle) = spawn_mock_copilot(mock_err).await;
        std::env::set_var(ENV_COPILOT_URL, format!("http://{addr}/rpc"));
        let err = forward_library_call("library.set_hot_cues", serde_json::json!({}))
            .await
            .expect_err("upstream error envelope must surface");
        clear_env();
        let _ = handle.await;
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("Invalid params"));
        assert_eq!(
            err.data
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "track_id must be non-empty"
        );
    }

    #[tokio::test]
    async fn library_http_5xx_returns_engine_offline() {
        // Mock that returns 500 with no body — must surface as
        // engine_offline regardless of body parseability.
        let _g = env_lock();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        std::env::set_var(ENV_COPILOT_URL, format!("http://{addr}/rpc"));
        let err = forward_library_call("library.list_tracks", serde_json::json!({}))
            .await
            .expect_err("5xx must error");
        clear_env();
        let _ = handle.await;
        assert_eq!(err.code, super::super::error::ENGINE_OFFLINE);
        assert!(
            err.data
                .as_ref()
                .and_then(Value::as_str)
                .map(|s| s.contains("503"))
                .unwrap_or(false),
            "expected status in data: {err:?}"
        );
    }
}
