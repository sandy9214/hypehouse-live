//! JSON-RPC 2.0 framing for the engine bridge.
//!
//! Two surfaces:
//!
//! 1. `RpcRequest` / `RpcResponse` / `RpcNotification` — wire envelopes
//!    serialized by serde over WebSocket text frames. The TS UI and the
//!    Python copilot both speak the same shape.
//! 2. `EngineHandle` — the in-process actor the bridge dispatches into.
//!    Owns the event log + state. Cloneable so each WS client task can
//!    drive it. State changes broadcast on a tokio `broadcast::Sender`
//!    so every connected client receives `engine.state_changed`.
//!
//! Why the handle lives here
//! -------------------------
//! Keeping the RPC method dispatch + the handle in the same module makes
//! the contract surface obvious: every supported method has a matching
//! `EngineHandle` call. If we ever split this into a separate `engine`
//! module that lives outside the bridge, the dispatch is still the
//! single place that translates JSON-RPC into a typed call.

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::state::{EngineState, Event, EventKind, EventSource};

use super::error::RpcError;

// ---------------------------------------------------------------------
// JSON-RPC 2.0 wire envelopes
// ---------------------------------------------------------------------

pub const JSONRPC_VERSION: &str = "2.0";

/// Either a JSON number or string id. Notifications omit the id.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum RpcId {
    Num(i64),
    Str(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// `None` means this is a *notification* — no response expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RpcId>,
}

impl RpcRequest {
    pub fn is_valid(&self) -> bool {
        self.jsonrpc == JSONRPC_VERSION && !self.method.is_empty()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    pub id: Option<RpcId>,
}

impl RpcResponse {
    pub fn ok(id: Option<RpcId>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn err(id: Option<RpcId>, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            result: None,
            error: Some(error),
            id,
        }
    }
}

/// Server → client notification (no `id`, no response expected).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

impl RpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method.into(),
            params,
        }
    }
}

// ---------------------------------------------------------------------
// Method names — single source of truth so client & server can't drift.
// ---------------------------------------------------------------------

pub mod method {
    pub const SUBMIT_EVENT: &str = "engine.submit_event";
    pub const SNAPSHOT: &str = "engine.snapshot";
    pub const EVENT_LOG: &str = "engine.event_log";
    pub const HEALTH: &str = "engine.health";

    pub const NOTIFY_STATE_CHANGED: &str = "engine.state_changed";
    pub const NOTIFY_AUDIO_ALERT: &str = "engine.audio_alert";
}

// ---------------------------------------------------------------------
// Engine handle — actor surface dispatched into by the WS server.
// ---------------------------------------------------------------------

/// Per-server metrics. Updated from the WS layer.
#[derive(Debug, Default)]
pub struct BridgeMetrics {
    pub audio_xrun_count: std::sync::atomic::AtomicU64,
    pub ws_clients_connected: std::sync::atomic::AtomicU64,
    pub ring_pending: std::sync::atomic::AtomicU64,
}

/// Notification published on state changes / audio alerts.
///
/// `StateChanged.state` is boxed because every broadcast subscriber
/// clones the entire variant on receive; keeping the discriminant small
/// avoids a per-subscriber stack copy of an `EngineState` (~800 bytes
/// today, growing as decks/effects expand).
#[derive(Debug, Clone)]
pub enum BridgeNotice {
    StateChanged {
        state: Box<EngineState>,
        last_event_id: u64,
    },
    AudioAlert {
        kind: String,
        details: String,
    },
}

/// The actor every WS client task drives. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    state: std::sync::Mutex<EngineState>,
    log: std::sync::Mutex<Vec<Event>>,
    next_event_id: std::sync::atomic::AtomicU64,
    notices: broadcast::Sender<BridgeNotice>,
    metrics: BridgeMetrics,
    boot_instant: Instant,
}

impl EngineHandle {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel::<BridgeNotice>(1024);
        let inner = EngineInner {
            state: std::sync::Mutex::new(EngineState::default()),
            log: std::sync::Mutex::new(Vec::new()),
            next_event_id: std::sync::atomic::AtomicU64::new(1),
            notices: tx,
            metrics: BridgeMetrics::default(),
            boot_instant: Instant::now(),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Subscribe to bridge notifications. Each WS client task gets its
    /// own receiver — `broadcast` handles fan-out + lag detection.
    pub fn subscribe(&self) -> broadcast::Receiver<BridgeNotice> {
        self.inner.notices.subscribe()
    }

    pub fn metrics(&self) -> &BridgeMetrics {
        &self.inner.metrics
    }

    /// Public for the WS layer to publish audio alerts (xruns, etc.).
    pub fn publish_audio_alert(&self, kind: impl Into<String>, details: impl Into<String>) {
        // `send` returns Err only when zero subscribers — fine, drop.
        let _ = self.inner.notices.send(BridgeNotice::AudioAlert {
            kind: kind.into(),
            details: details.into(),
        });
    }

    /// Snapshot of the current state. Pure read — never blocks audio.
    pub fn snapshot(&self) -> EngineState {
        self.inner
            .state
            .lock()
            .expect("engine state poisoned")
            .clone()
    }

    /// Last assigned event id (== log length). Useful for diffing.
    pub fn last_event_id(&self) -> u64 {
        self.inner
            .next_event_id
            .load(std::sync::atomic::Ordering::SeqCst)
            - 1
    }

    /// Slice of the event log starting after `since` (exclusive), at
    /// most `limit` items. `since = 0` returns the full prefix.
    pub fn event_log(&self, since: u64, limit: u32) -> Vec<Event> {
        let log = self.inner.log.lock().expect("engine log poisoned");
        log.iter()
            .filter(|e| e.id > since)
            .take(limit as usize)
            .cloned()
            .collect()
    }

    pub fn health(&self) -> serde_json::Value {
        let uptime_ms = self.inner.boot_instant.elapsed().as_millis() as u64;
        let m = &self.inner.metrics;
        let ord = std::sync::atomic::Ordering::Relaxed;
        serde_json::json!({
            "uptime_ms": uptime_ms,
            "audio_xrun_count": m.audio_xrun_count.load(ord),
            "ws_clients_connected": m.ws_clients_connected.load(ord),
            "ring_pending": m.ring_pending.load(ord),
        })
    }

    /// Apply an event from the wire. The caller supplies the
    /// `EventKind` + `EventSource`; the engine stamps id + timestamp so
    /// they're monotonic in receive order. Returns the new state id.
    pub fn submit_event_kind(&self, kind: EventKind, source: EventSource) -> u64 {
        let id = self
            .inner
            .next_event_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let ts_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let ev = Event {
            id,
            ts_micros,
            source,
            kind,
        };

        let next_state = {
            let mut s = self.inner.state.lock().expect("engine state poisoned");
            let next = s.apply(&ev);
            *s = next.clone();
            next
        };

        self.inner.log.lock().expect("engine log poisoned").push(ev);

        let _ = self.inner.notices.send(BridgeNotice::StateChanged {
            state: Box::new(next_state),
            last_event_id: id,
        });

        id
    }
}

impl Default for EngineHandle {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------
// Method dispatch — RpcRequest → RpcResponse.
// ---------------------------------------------------------------------

/// Params for `engine.submit_event`. The wire format accepts either the
/// full typed `EventKind` enum (preferred — round-trips with the engine's
/// own model) OR a wrapped object with explicit `kind` + optional
/// `source`. Both keep the bridge robust against client conventions.
#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum SubmitEventParams {
    Wrapped {
        kind: EventKind,
        #[serde(default)]
        source: Option<EventSource>,
    },
    Bare(EventKind),
}

#[derive(Deserialize, Debug)]
struct EventLogParams {
    #[serde(default)]
    since: u64,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    1024
}

/// Translate a JSON-RPC request into a response by dispatching into the
/// engine handle. No I/O — pure CPU.
pub fn dispatch(engine: &EngineHandle, req: RpcRequest) -> RpcResponse {
    if !req.is_valid() {
        return RpcResponse::err(
            req.id,
            RpcError::invalid_request("jsonrpc must be \"2.0\" and method non-empty"),
        );
    }

    let id = req.id.clone();
    match req.method.as_str() {
        method::SUBMIT_EVENT => {
            let params = match req.params {
                Some(v) => v,
                None => return RpcResponse::err(id, RpcError::invalid_params("missing params")),
            };
            let parsed: Result<SubmitEventParams, _> = serde_json::from_value(params);
            let (kind, source) = match parsed {
                Ok(SubmitEventParams::Wrapped { kind, source }) => {
                    (kind, source.unwrap_or(EventSource::Ui))
                }
                Ok(SubmitEventParams::Bare(kind)) => (kind, EventSource::Ui),
                Err(e) => return RpcResponse::err(id, RpcError::invalid_params(e.to_string())),
            };
            let state_id = engine.submit_event_kind(kind, source);
            RpcResponse::ok(
                id,
                serde_json::json!({ "accepted": true, "state_id": state_id }),
            )
        }
        method::SNAPSHOT => {
            let snap = engine.snapshot();
            match serde_json::to_value(snap) {
                Ok(v) => RpcResponse::ok(id, v),
                Err(e) => RpcResponse::err(id, RpcError::internal(e.to_string())),
            }
        }
        method::EVENT_LOG => {
            let params: EventLogParams = match req.params {
                Some(v) => match serde_json::from_value(v) {
                    Ok(p) => p,
                    Err(e) => return RpcResponse::err(id, RpcError::invalid_params(e.to_string())),
                },
                None => EventLogParams {
                    since: 0,
                    limit: default_limit(),
                },
            };
            let slice = engine.event_log(params.since, params.limit);
            match serde_json::to_value(slice) {
                Ok(v) => RpcResponse::ok(id, v),
                Err(e) => RpcResponse::err(id, RpcError::internal(e.to_string())),
            }
        }
        method::HEALTH => RpcResponse::ok(id, engine.health()),
        other => RpcResponse::err(id, RpcError::method_not_found(other)),
    }
}

/// Build a `state_changed` notification frame.
pub fn state_changed_notification(state: &EngineState, last_event_id: u64) -> RpcNotification {
    RpcNotification::new(
        method::NOTIFY_STATE_CHANGED,
        serde_json::json!({ "state": state, "last_event_id": last_event_id }),
    )
}

/// Build an `audio_alert` notification frame.
pub fn audio_alert_notification(kind: &str, details: &str) -> RpcNotification {
    RpcNotification::new(
        method::NOTIFY_AUDIO_ALERT,
        serde_json::json!({ "kind": kind, "details": details }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeckId, EventKind};

    fn engine() -> EngineHandle {
        EngineHandle::new()
    }

    fn submit(method: &str, params: Value, id: i64) -> RpcRequest {
        RpcRequest {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method.into(),
            params: Some(params),
            id: Some(RpcId::Num(id)),
        }
    }

    #[test]
    fn submit_event_accepts_deck_play_and_advances_state_id() {
        let e = engine();
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "kind": { "DeckPlay": { "deck": "A" } } }),
            1,
        );
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["accepted"], Value::Bool(true));
        assert_eq!(result["state_id"].as_u64(), Some(1));
        assert!(e.snapshot().deck_a.playing);
    }

    #[test]
    fn submit_event_unknown_kind_returns_invalid_params() {
        let e = engine();
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "kind": { "NotAnEvent": {} } }),
            2,
        );
        let resp = dispatch(&e, req);
        assert_eq!(
            resp.error.unwrap().code,
            super::super::error::INVALID_PARAMS
        );
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let e = engine();
        let req = submit("engine.no_such_method", Value::Null, 3);
        let resp = dispatch(&e, req);
        assert_eq!(
            resp.error.unwrap().code,
            super::super::error::METHOD_NOT_FOUND
        );
    }

    #[test]
    fn snapshot_round_trips_engine_state() {
        let e = engine();
        e.submit_event_kind(EventKind::DeckPlay { deck: DeckId::A }, EventSource::Ui);
        let req = submit(method::SNAPSHOT, Value::Null, 4);
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none());
        let state: EngineState = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(state.deck_a.playing);
    }

    #[test]
    fn event_log_returns_slice_after_since() {
        let e = engine();
        for _ in 0..5 {
            e.submit_event_kind(EventKind::DeckPlay { deck: DeckId::A }, EventSource::Ui);
        }
        let req = submit(
            method::EVENT_LOG,
            serde_json::json!({ "since": 2, "limit": 10 }),
            5,
        );
        let resp = dispatch(&e, req);
        let events: Vec<Event> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].id, 3);
    }

    #[test]
    fn invalid_jsonrpc_version_is_rejected() {
        let req = RpcRequest {
            jsonrpc: "1.0".into(),
            method: method::SNAPSHOT.into(),
            params: None,
            id: Some(RpcId::Num(99)),
        };
        let e = engine();
        let resp = dispatch(&e, req);
        assert_eq!(
            resp.error.unwrap().code,
            super::super::error::INVALID_REQUEST
        );
    }

    #[test]
    fn state_changed_notification_emitted_on_submit() {
        let e = engine();
        let mut rx = e.subscribe();
        e.submit_event_kind(EventKind::DeckPlay { deck: DeckId::A }, EventSource::Ui);
        // try_recv works on broadcast::Receiver — the message is buffered.
        let n = rx.try_recv().expect("notification should be queued");
        match n {
            BridgeNotice::StateChanged {
                state,
                last_event_id,
            } => {
                assert_eq!(last_event_id, 1);
                assert!(state.deck_a.playing);
            }
            BridgeNotice::AudioAlert { .. } => panic!("expected StateChanged"),
        }
    }

    #[test]
    fn health_reports_uptime_and_metrics_keys() {
        let e = engine();
        let v = e.health();
        for key in [
            "uptime_ms",
            "audio_xrun_count",
            "ws_clients_connected",
            "ring_pending",
        ] {
            assert!(v.get(key).is_some(), "missing key: {key}");
        }
    }

    #[test]
    fn health_method_dispatches() {
        let e = engine();
        let req = submit(method::HEALTH, Value::Null, 7);
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none());
        let v = resp.result.unwrap();
        assert!(v.get("uptime_ms").is_some());
    }

    #[test]
    fn submit_event_accepts_bare_kind_shape() {
        let e = engine();
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "DeckPlay": { "deck": "A" } }),
            8,
        );
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "{:?}", resp.error);
        assert!(e.snapshot().deck_a.playing);
    }
}
