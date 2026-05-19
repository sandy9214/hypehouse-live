//! JSON-RPC 2.0 error envelopes.
//!
//! Codes per the JSON-RPC spec (https://www.jsonrpc.org/specification):
//!
//! * `-32700` Parse error      — invalid JSON received.
//! * `-32600` Invalid request  — payload is not a valid JSON-RPC request.
//! * `-32601` Method not found — method does not exist.
//! * `-32602` Invalid params   — method exists but params shape is wrong.
//! * `-32603` Internal error   — server-side reducer / state failure.
//!
//! Application-defined codes live in `-32000..=-32099` (reserved by spec).
//!
//! Engine-specific application codes:
//! * `-32000` `ENGINE_OFFLINE` — control-loop event channel rejected the
//!   event (full / disconnected).
//! * `-32001` `ENGINE_SINK_UNWIRED` — handle has no `event_sink`
//!   configured; submit_event cannot reach the control loop. Snapshot /
//!   event_log / health still work.
//! * `-32002` `AUTH_REJECTED` — bearer-token auth missing/wrong (rare;
//!   handshake usually rejects with HTTP 401 before this code is emitted).
//! * `-32003` `RATE_LIMITED` — per-client token bucket exhausted on
//!   `engine.submit_event`. The error `data` field carries
//!   `{ retry_after_ms: u64 }` so the client can back off.

use serde::{Deserialize, Serialize};

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
/// Application-defined: control-loop event channel is full or disconnected.
pub const ENGINE_OFFLINE: i32 = -32000;
/// Application-defined: handle has no event_sink wired (typical in unit
/// tests using `EngineHandle::new()`).
pub const ENGINE_SINK_UNWIRED: i32 = -32001;
/// Application-defined: bearer-token auth missing or wrong.
pub const AUTH_REJECTED: i32 = -32002;
/// Application-defined: per-client token bucket on
/// `engine.submit_event` is exhausted. The `data` field carries
/// `{ retry_after_ms: u64 }` indicating how long until the next token
/// regenerates. See `bridge::ratelimit`.
pub const RATE_LIMITED: i32 = -32003;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<serde_json::Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn parse_error(detail: impl Into<String>) -> Self {
        Self::new(PARSE_ERROR, "Parse error").with_data(detail.into())
    }

    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, "Invalid Request").with_data(detail.into())
    }

    pub fn method_not_found(method: impl Into<String>) -> Self {
        Self::new(METHOD_NOT_FOUND, "Method not found").with_data(method.into())
    }

    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, "Invalid params").with_data(detail.into())
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, "Internal error").with_data(detail.into())
    }

    pub fn auth_rejected(detail: impl Into<String>) -> Self {
        Self::new(AUTH_REJECTED, "Authentication rejected").with_data(detail.into())
    }

    /// Build a `-32003 RATE_LIMITED` error with the structured
    /// `{ retry_after_ms }` data payload. Returned when the per-client
    /// token bucket on `engine.submit_event` is exhausted.
    pub fn rate_limited(retry_after_ms: u64) -> Self {
        Self {
            code: RATE_LIMITED,
            message: "Rate limited".into(),
            data: Some(serde_json::json!({ "retry_after_ms": retry_after_ms })),
        }
    }

    /// Build a `-32000 engine offline` error. Returned when the bridge
    /// could not forward an event onto the control-loop channel (full or
    /// disconnected). Callers may retry after backoff.
    pub fn engine_offline(detail: impl Into<String>) -> Self {
        Self::new(ENGINE_OFFLINE, "engine offline").with_data(detail.into())
    }

    /// Build a `-32001 engine sink not wired` error. Returned when the
    /// `EngineHandle` was built without an event sink (typical for unit
    /// tests). Snapshot / event_log / health calls still succeed.
    pub fn engine_sink_unwired(detail: impl Into<String>) -> Self {
        Self::new(ENGINE_SINK_UNWIRED, "engine sink not wired").with_data(detail.into())
    }

    fn with_data(mut self, data: String) -> Self {
        self.data = Some(serde_json::Value::String(data));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_match_spec() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(INVALID_REQUEST, -32600);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL_ERROR, -32603);
    }

    #[test]
    fn engine_app_codes_in_reserved_range() {
        // JSON-RPC 2.0 reserves -32000..=-32099 for the server side.
        assert_eq!(ENGINE_OFFLINE, -32000);
        assert_eq!(ENGINE_SINK_UNWIRED, -32001);
        assert_eq!(AUTH_REJECTED, -32002);
        assert_eq!(RATE_LIMITED, -32003);
    }

    #[test]
    fn rate_limited_carries_structured_retry_after_ms() {
        let e = RpcError::rate_limited(42);
        assert_eq!(e.code, RATE_LIMITED);
        assert_eq!(e.message, "Rate limited");
        let data = e.data.clone().expect("rate_limited carries data");
        assert_eq!(
            data.get("retry_after_ms").and_then(|v| v.as_u64()),
            Some(42),
            "retry_after_ms field must serialize as a u64 number"
        );
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("-32003"));
        assert!(s.contains("retry_after_ms"));
    }

    #[test]
    fn engine_offline_serializes_with_code_and_message() {
        let e = RpcError::engine_offline("channel full");
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("-32000"));
        assert!(s.contains("engine offline"));
        assert!(s.contains("channel full"));
    }

    #[test]
    fn engine_sink_unwired_serializes_with_code_and_message() {
        let e = RpcError::engine_sink_unwired("test handle");
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("-32001"));
        assert!(s.contains("engine sink not wired"));
    }

    #[test]
    fn invalid_request_serializes_with_data() {
        let e = RpcError::invalid_request("missing id");
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("-32600"));
        assert!(s.contains("Invalid Request"));
        assert!(s.contains("missing id"));
    }

    #[test]
    fn error_without_data_omits_data_field() {
        let e = RpcError::new(INTERNAL_ERROR, "boom");
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("\"data\""));
    }
}
