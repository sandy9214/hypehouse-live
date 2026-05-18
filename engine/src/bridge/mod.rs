//! WebSocket bridge + JSON-RPC schema between the Rust engine, the TS
//! frontend, and the Python copilot service.
//!
//! See `docs/api/ws-protocol.md` for the wire format reference and
//! ADR-001 (stack choice) / ADR-003 (event-sourced state) for the
//! design rationale.
//!
//! Public surface
//! --------------
//! * [`EngineHandle`] — shared, cloneable handle into the event log +
//!   reducer + broadcast channel. Hand the same instance to the audio
//!   thread / MIDI listener so their inputs also fan out to all
//!   connected clients via `engine.state_changed` notifications.
//! * [`spawn`] — start the WS server on a background tokio task and
//!   get back a [`BridgeServer`] handle for `.shutdown().await`.
//! * [`BridgeConfig`] — bind addr + auth, populated via
//!   [`BridgeConfig::from_env`] in production or directly in tests.
//!
//! Auth model
//! ----------
//! When `HYPEHOUSE_BRIDGE_TOKEN` is set, every WS handshake must include
//! `Authorization: Bearer <token>`. When unset, the server binds to
//! loopback only and accepts every connection. See [`auth::AuthConfig`].
//!
//! Error codes
//! -----------
//! Standard JSON-RPC: `-32600` invalid request (also used for malformed
//! JSON per this engine's framing requirement), `-32601` method not
//! found, `-32602` invalid params, `-32603` internal.
//!
//! Engine application codes (`-32000..=-32099`):
//! * `-32000` `ENGINE_OFFLINE`      — control-loop channel full/disconnected.
//! * `-32001` `ENGINE_SINK_UNWIRED` — handle has no event sink wired.
//! * `-32002` `AUTH_REJECTED`       — rare; handshake usually 401s first.
//!
//! See [`error`] for full enumeration.

pub mod auth;
pub mod error;
pub mod rpc;
pub mod ws_server;

pub use auth::AuthConfig;
pub use error::{
    RpcError, AUTH_REJECTED, ENGINE_OFFLINE, ENGINE_SINK_UNWIRED, INTERNAL_ERROR, INVALID_PARAMS,
    INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
};
pub use rpc::{
    audio_alert_notification, dispatch, dispatch_auth_hello, dispatch_with_auth, method,
    state_changed_notification, AuthState, BridgeMetrics, BridgeNotice, EngineHandle, RpcId,
    RpcNotification, RpcRequest, RpcResponse, JSONRPC_VERSION,
};
pub use ws_server::{spawn, BridgeConfig, BridgeServer, DEFAULT_PORT};
