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

use std::sync::atomic::{AtomicI16, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossbeam::channel::{Sender, TrySendError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::audio::clock::SharedClock;
use crate::audio::decode_gain_reduction_db;
use crate::audio::effects::{
    descriptors, EffectId, ParamDescriptor, EFFECT_ECHO, EFFECT_FILTER, EFFECT_GATE, EFFECT_REVERB,
};
use crate::audio::ClockSource;
use crate::state::{DeckId, EngineState, Event, EventKind, EventSource};

use super::auth::AuthConfig;
use super::error::RpcError;
use super::library_proxy;

/// Method-name prefix that triggers the library proxy hop.
///
/// Any RPC whose name starts with `library.` is forwarded by
/// [`dispatch_with_auth_async`] to the copilot service via
/// [`library_proxy::forward_library_call`] instead of being dispatched
/// against the in-process [`EngineHandle`]. Centralising the prefix here
/// keeps the routing rule in one place.
pub const LIBRARY_PREFIX: &str = "library.";

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
    /// ADR-006 — effect manifest. Stubbed: returns `[]` until the full
    /// manifest plumbing lands (issue TBD).
    pub const LIST_EFFECTS: &str = "engine.list_effects";
    /// History — enumerate persisted past sessions on disk. Read-only.
    /// Backed by `crate::persistence::sessions::list_sessions`. See
    /// `docs/api/ws-protocol.md` for the response shape.
    pub const LIST_SESSIONS: &str = "engine.list_sessions";
    /// History — replay the events of a single past session and return
    /// the resulting `EngineState` snapshot. Does NOT mutate live
    /// state. Backed by `crate::persistence::sessions::replay_session`.
    pub const REPLAY_SESSION: &str = "engine.replay_session";
    /// In-band bearer-token auth for browser WS clients that cannot set
    /// the `Authorization` header at upgrade. See `auth::AuthState`.
    pub const AUTH_HELLO: &str = "auth.hello";

    pub const NOTIFY_STATE_CHANGED: &str = "engine.state_changed";
    pub const NOTIFY_AUDIO_ALERT: &str = "engine.audio_alert";
    /// Out-of-band decode failure. Emitted by the control thread when
    /// `DecodeService::open` errors on a `DeckLoad`. Surfaces in the UI
    /// as a transient toast (no state mutation — the deck simply stays
    /// unloaded). See `docs/api/ws-protocol.md` "Engine notifications:
    /// decode_error".
    pub const NOTIFY_DECODE_ERROR: &str = "engine.decode_error";
}

// ---------------------------------------------------------------------
// Per-connection auth state — drives the pending-auth gate.
// ---------------------------------------------------------------------

/// State machine that gates per-connection RPC dispatch.
///
/// Browser WebSocket clients cannot attach an `Authorization: Bearer …`
/// header at HTTP upgrade. They connect in [`AuthState::PendingAuth`] and
/// must call `auth.hello` as the very first JSON-RPC method. Native
/// clients (Tauri, Rust integration tests) keep using the header path and
/// start in [`AuthState::Authed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    /// No bearer-token presented yet. Only `auth.hello` is dispatched;
    /// every other method short-circuits with `AUTH_REJECTED`.
    PendingAuth,
    /// Bearer-token already verified (via header or via `auth.hello`).
    Authed,
}

impl AuthState {
    pub fn is_authed(self) -> bool {
        matches!(self, AuthState::Authed)
    }
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
        /// Live master-bus limiter gain reduction in dB at the moment
        /// this notification was built. Always `≤ 0`. Sourced from the
        /// audio thread's shared atomic; sampled once per
        /// `state_changed` so UI clients can render the meter live
        /// without polling a separate channel. `0.0` when no GR atomic
        /// is wired (e.g. unit tests that hit the bare
        /// `EngineHandle::new()` constructor).
        master_limiter_gain_reduction_db: f32,
        /// Active tempo source at the moment this notification was
        /// built. Sourced from the shared clock's atomic — same
        /// rationale as `master_limiter_gain_reduction_db`: a live
        /// audio-thread measurement, NOT part of the event-sourced
        /// reducer state. `ClockSource::Internal` when no shared clock
        /// is wired (the bare `EngineHandle::new()` test path).
        clock_source: ClockSource,
    },
    AudioAlert {
        kind: String,
        details: String,
    },
    /// Surface a decode-pipeline failure as an out-of-band UI toast.
    ///
    /// Emitted by the control thread when `DecodeService::open` returns
    /// `Err` during a `DeckLoad` event. The deck does NOT mutate state
    /// (the existing log-and-suppress contract is preserved); we just
    /// inform connected clients so the operator sees the failure instead
    /// of a silent no-op load.
    ///
    /// `category` lets the UI badge the toast (file_not_found,
    /// format_unsupported, decoder_error, …). `error` carries the
    /// stringified `DecodeError` for debugging context.
    DecodeError {
        deck: DeckId,
        track_id: String,
        category: String,
        error: String,
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
    /// Optional control-loop event channel. When `Some`, `submit_event`
    /// RPCs `try_send` into it (non-blocking — full / disconnected maps
    /// to `-32000 engine offline`). When `None`, `submit_event` returns
    /// `-32001 engine sink not wired` so unit tests that use the bare
    /// `EngineHandle::new()` constructor still get a structured error
    /// instead of a panic — and snapshot/event_log/health remain usable.
    event_sink: Option<Sender<Event>>,
    /// Optional shared handle on the master-bus limiter's
    /// gain-reduction readout (audio thread → bridge thread). When
    /// `Some`, every `BridgeNotice::StateChanged` we publish stamps a
    /// fresh `master_limiter_gain_reduction_db` read off this atomic so
    /// the UI meter can render live. `None` in tests that don't spin
    /// up a real audio thread; the wire payload then carries `0.0`.
    master_limiter_gr: std::sync::Mutex<Option<Arc<AtomicI16>>>,
    /// Optional handle on the engine's `SharedClock`. When `Some`, every
    /// `BridgeNotice::StateChanged` we publish stamps the active
    /// [`ClockSource`] read off the clock's atomic so the UI BPM-lock
    /// badge can render without polling a separate channel. `None` in
    /// tests that don't spin up a real audio thread; the wire payload
    /// then carries `"internal"`.
    shared_clock: std::sync::Mutex<Option<SharedClock>>,
}

impl EngineHandle {
    /// Zero-arg constructor — leaves the event sink unwired. Used by
    /// the integration & unit tests that exercise snapshot / event_log /
    /// health without spinning up the control loop. Live runs should
    /// call [`EngineHandle::with_event_sink`] instead.
    pub fn new() -> Self {
        Self::build(None)
    }

    /// Build a handle wired to the control-loop event channel. The
    /// bridge will forward every accepted `engine.submit_event` RPC
    /// payload onto `sink` via `try_send` (non-blocking).
    pub fn with_event_sink(sink: Sender<Event>) -> Self {
        Self::build(Some(sink))
    }

    fn build(event_sink: Option<Sender<Event>>) -> Self {
        let (tx, _) = broadcast::channel::<BridgeNotice>(1024);
        let inner = EngineInner {
            state: std::sync::Mutex::new(EngineState::default()),
            log: std::sync::Mutex::new(Vec::new()),
            next_event_id: std::sync::atomic::AtomicU64::new(1),
            notices: tx,
            metrics: BridgeMetrics::default(),
            boot_instant: Instant::now(),
            event_sink,
            master_limiter_gr: std::sync::Mutex::new(None),
            shared_clock: std::sync::Mutex::new(None),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Wire the audio thread's master-bus limiter gain-reduction
    /// readout into this handle. Called once at startup (see
    /// `main.rs`); the bridge then samples the atomic for every
    /// outgoing `engine.state_changed` notification. Re-calling
    /// replaces the previous handle so a future hot-swap of the audio
    /// stack is safe.
    pub fn attach_master_limiter_gr(&self, gr: Arc<AtomicI16>) {
        let mut slot = self
            .inner
            .master_limiter_gr
            .lock()
            .expect("engine master_limiter_gr poisoned");
        *slot = Some(gr);
    }

    /// Read the live master-limiter gain reduction in dB. Returns
    /// `0.0` when no audio thread is wired (test/bare mode).
    pub fn master_limiter_gain_reduction_db(&self) -> f32 {
        let slot = self
            .inner
            .master_limiter_gr
            .lock()
            .expect("engine master_limiter_gr poisoned");
        match slot.as_ref() {
            None => 0.0,
            Some(a) => decode_gain_reduction_db(a.load(Ordering::Relaxed)),
        }
    }

    /// Wire the engine's `SharedClock` into this handle so every
    /// outgoing `engine.state_changed` notification samples the active
    /// [`ClockSource`]. Called once at startup (`main.rs`); re-calling
    /// replaces the previous handle so a future hot-swap of the clock
    /// is safe.
    pub fn attach_shared_clock(&self, clock: SharedClock) {
        let mut slot = self
            .inner
            .shared_clock
            .lock()
            .expect("engine shared_clock poisoned");
        *slot = Some(clock);
    }

    /// Read the active tempo source. Returns `ClockSource::Internal`
    /// when no shared clock is wired (test/bare mode).
    pub fn clock_source(&self) -> ClockSource {
        let slot = self
            .inner
            .shared_clock
            .lock()
            .expect("engine shared_clock poisoned");
        match slot.as_ref() {
            None => ClockSource::Internal,
            Some(c) => c.clock_source(),
        }
    }

    /// Whether this handle has a wired event sink (i.e. live mode vs.
    /// bare-test mode). Used by dispatch to choose the error response.
    pub fn has_event_sink(&self) -> bool {
        self.inner.event_sink.is_some()
    }

    /// Forward `event` onto the control-loop channel without blocking.
    /// Returns:
    /// * `Ok(())` on accept,
    /// * `Err(RpcError::engine_offline(...))` if the channel is full or
    ///   the receiver was dropped,
    /// * `Err(RpcError::engine_sink_unwired(...))` if no sink is wired.
    pub fn forward_event(&self, event: Event) -> Result<(), RpcError> {
        match &self.inner.event_sink {
            None => Err(RpcError::engine_sink_unwired(
                "EngineHandle constructed without an event sink",
            )),
            Some(sink) => match sink.try_send(event) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => Err(RpcError::engine_offline("event channel full")),
                Err(TrySendError::Disconnected(_)) => Err(RpcError::engine_offline(
                    "event channel disconnected (control loop exited)",
                )),
            },
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

    /// Publish a `decode_error` notification. Called by the control
    /// thread when the translator's `DecodeService::open` errors on a
    /// `DeckLoad` event. Non-blocking — drops silently if no clients are
    /// currently subscribed (e.g. unit tests, paused UI).
    pub fn publish_decode_error(
        &self,
        deck: DeckId,
        track_id: impl Into<String>,
        category: impl Into<String>,
        error: impl Into<String>,
    ) {
        let _ = self.inner.notices.send(BridgeNotice::DecodeError {
            deck,
            track_id: track_id.into(),
            category: category.into(),
            error: error.into(),
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

        let gr_db = self.master_limiter_gain_reduction_db();
        let clock_source = self.clock_source();
        let _ = self.inner.notices.send(BridgeNotice::StateChanged {
            state: Box::new(next_state),
            last_event_id: id,
            master_limiter_gain_reduction_db: gr_db,
            clock_source,
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

/// Params for `engine.replay_session`. Object-typed so future fields
/// (e.g. `up_to_event_id`) slot in without breaking older clients.
#[derive(Deserialize, Debug)]
struct ReplaySessionParams {
    session_id: String,
}

/// Params for `auth.hello` — the in-band bearer-token handshake.
#[derive(Deserialize, Debug)]
struct AuthHelloParams {
    token: String,
}

/// Build the success result body for `auth.hello`. Each call gets a
/// fresh micros-since-UNIX timestamp as a lightweight session marker so
/// the client can correlate logs without needing a UUID dep.
fn auth_hello_success_result() -> Value {
    let session = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    serde_json::json!({ "authed": true, "session": session })
}

/// Dispatch helper that handles only the in-band `auth.hello` handshake.
///
/// Returns the response envelope plus the **new** auth state. Caller
/// (WS server task) threads the new state back into its loop so
/// subsequent methods on the same connection see the transition.
///
/// Idempotency: calling `auth.hello` from an already-authed connection
/// re-validates the token and returns success (no state regression) so
/// retries / reconnect-and-replay scenarios stay safe.
pub fn dispatch_auth_hello(
    auth: &AuthConfig,
    state: AuthState,
    req: RpcRequest,
) -> (RpcResponse, AuthState) {
    let id = req.id.clone();
    let params = match req.params.clone() {
        Some(v) => v,
        None => {
            return (
                RpcResponse::err(id, RpcError::invalid_params("missing params")),
                state,
            );
        }
    };
    let parsed: Result<AuthHelloParams, _> = serde_json::from_value(params);
    let token = match parsed {
        Ok(p) => p.token,
        Err(e) => {
            return (
                RpcResponse::err(id, RpcError::invalid_params(e.to_string())),
                state,
            );
        }
    };

    // `check_header` is the same comparator the WS handshake uses — feed
    // it a `Bearer <token>` string so the token-equality path is shared
    // and we get its constant-time compare for free.
    let header_value = format!("Bearer {token}");
    if auth.check_header(Some(&header_value)) {
        (
            RpcResponse::ok(id, auth_hello_success_result()),
            AuthState::Authed,
        )
    } else {
        (
            RpcResponse::err(id, RpcError::auth_rejected("invalid token")),
            state,
        )
    }
}

/// Per-connection dispatch entry-point used by the WS server.
///
/// Enforces the pending-auth gate: when `state == PendingAuth`, the only
/// method allowed through is `auth.hello`; everything else short-circuits
/// with `-32002 AUTH_REJECTED`. On success, returns the new state so the
/// caller can promote the connection without an explicit second
/// round-trip.
pub fn dispatch_with_auth(
    engine: &EngineHandle,
    auth: &AuthConfig,
    state: AuthState,
    req: RpcRequest,
) -> (RpcResponse, AuthState) {
    if !req.is_valid() {
        return (
            RpcResponse::err(
                req.id,
                RpcError::invalid_request("jsonrpc must be \"2.0\" and method non-empty"),
            ),
            state,
        );
    }

    if req.method == method::AUTH_HELLO {
        return dispatch_auth_hello(auth, state, req);
    }

    if state == AuthState::PendingAuth {
        return (
            RpcResponse::err(req.id, RpcError::auth_rejected("authentication required")),
            state,
        );
    }

    (dispatch(engine, req), state)
}

/// Async sibling of [`dispatch_with_auth`].
///
/// Adds one extra rule on top of the sync version: methods whose name
/// starts with [`LIBRARY_PREFIX`] are forwarded over HTTP to the copilot
/// service via [`library_proxy::forward_library_call`]. Everything else
/// falls through to the synchronous dispatcher, which still runs on the
/// caller's task (the work is pure CPU on the engine handle).
///
/// The auth gate is enforced **before** the library-proxy hop — the proxy
/// never receives a frame from a `PendingAuth` connection, so an
/// unauthenticated UI cannot trigger outbound HTTP traffic.
pub async fn dispatch_with_auth_async(
    engine: &EngineHandle,
    auth: &AuthConfig,
    state: AuthState,
    req: RpcRequest,
) -> (RpcResponse, AuthState) {
    if !req.is_valid() {
        return (
            RpcResponse::err(
                req.id,
                RpcError::invalid_request("jsonrpc must be \"2.0\" and method non-empty"),
            ),
            state,
        );
    }

    if req.method == method::AUTH_HELLO {
        return dispatch_auth_hello(auth, state, req);
    }

    if state == AuthState::PendingAuth {
        return (
            RpcResponse::err(req.id, RpcError::auth_rejected("authentication required")),
            state,
        );
    }

    if req.method.starts_with(LIBRARY_PREFIX) {
        let id = req.id.clone();
        let method = req.method.clone();
        let params = req.params.unwrap_or(Value::Null);
        let resp = match library_proxy::forward_library_call(&method, params).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, e),
        };
        return (resp, state);
    }

    (dispatch(engine, req), state)
}

/// Static list of registered (id, name) pairs the manifest exposes.
///
/// Kept inline here (not on the registry) because the wire-facing name
/// is a UI contract — the audio-thread side doesn't need the strings
/// and adding one is a bridge-layer choice.
const REGISTERED_EFFECTS: &[(EffectId, &str)] = &[
    (EFFECT_FILTER, "filter"),
    (EFFECT_ECHO, "echo"),
    (EFFECT_REVERB, "reverb"),
    (EFFECT_GATE, "gate"),
];

fn param_descriptor_to_json(d: &ParamDescriptor) -> Value {
    serde_json::json!({
        "name": d.name,
        "min": d.min,
        "max": d.max,
        "default": d.default,
    })
}

/// Build the `engine.list_effects` result body. Pure — no I/O, no
/// allocation outside the JSON nodes themselves.
fn list_effects_result() -> Value {
    let effects: Vec<Value> = REGISTERED_EFFECTS
        .iter()
        .map(|(id, name)| {
            let params: Vec<Value> = descriptors(*id)
                .iter()
                .map(param_descriptor_to_json)
                .collect();
            serde_json::json!({
                "id": *id,
                "name": *name,
                "params": params,
            })
        })
        .collect();
    serde_json::json!({ "effects": effects })
}

/// Translate a JSON-RPC request into a response by dispatching into the
/// engine handle. No I/O — pure CPU.
///
/// Note: this entry-point does **not** consult the in-band auth state.
/// Callers operating below the WS layer (unit tests, headless smoke
/// tools, internal integrations) bypass `auth.hello` deliberately — they
/// have already established trust by direct in-process access. The WS
/// server uses [`dispatch_with_auth`] instead and gates every per-method
/// call on `AuthState`.
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
            // Stamp id + ts on the bridge side so the control loop sees
            // monotonic ordering in receive order. The reducer is then
            // pure on the consumer side.
            let event_id = engine
                .inner
                .next_event_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let ts_micros = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0);
            let event = Event {
                id: event_id,
                ts_micros,
                source,
                kind,
            };
            match engine.forward_event(event) {
                Ok(()) => RpcResponse::ok(id, serde_json::json!({ "accepted": true })),
                Err(e) => RpcResponse::err(id, e),
            }
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
        // ADR-006 — effect manifest. Pulls the static descriptor list
        // from `crate::audio::effects::descriptors()` for each
        // registered effect id and emits a JSON payload the UI can
        // render directly. The shape is `{ "effects": [ ... ] }` so
        // the response is a JSON object (forward-compatible with
        // future top-level fields like `version`) rather than a bare
        // array. See docs/api/ws-protocol.md.
        method::LIST_EFFECTS => RpcResponse::ok(id, list_effects_result()),
        // History — read-only enumeration of past session dirs. Pure
        // disk I/O; never touches live engine state. Errors are
        // bubbled as `-32603 internal` because the caller did nothing
        // wrong — the storage layer is degraded.
        method::LIST_SESSIONS => match crate::persistence::sessions::list_sessions() {
            Ok(sessions) => {
                match serde_json::to_value(crate::persistence::sessions::ListSessionsResult {
                    sessions,
                }) {
                    Ok(v) => RpcResponse::ok(id, v),
                    Err(e) => RpcResponse::err(id, RpcError::internal(e.to_string())),
                }
            }
            Err(e) => RpcResponse::err(id, RpcError::internal(format!("{e:#}"))),
        },
        // History — fold one session's events.jsonl through
        // `replay_state` and return the snapshot. Read-only.
        method::REPLAY_SESSION => {
            let params: ReplaySessionParams = match req.params {
                Some(v) => match serde_json::from_value(v) {
                    Ok(p) => p,
                    Err(e) => return RpcResponse::err(id, RpcError::invalid_params(e.to_string())),
                },
                None => return RpcResponse::err(id, RpcError::invalid_params("missing params")),
            };
            match crate::persistence::sessions::replay_session(&params.session_id) {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(v) => RpcResponse::ok(id, v),
                    Err(e) => RpcResponse::err(id, RpcError::internal(e.to_string())),
                },
                Err(e) => RpcResponse::err(id, RpcError::invalid_params(format!("{e:#}"))),
            }
        }
        other => RpcResponse::err(id, RpcError::method_not_found(other)),
    }
}

/// Build a `state_changed` notification frame.
///
/// `master_limiter_gain_reduction_db` and `clock_source` are published
/// alongside `state` as sibling fields (not on `EngineState` itself)
/// because both are live audio-thread measurements — not part of the
/// event-sourced reducer state. The UI store reads them from the wire
/// payload and merges into the local mirror so the gain-reduction meter
/// + the BPM-lock badge stay fresh on every notification tick.
///
/// `clock_source` serializes as the stable kebab-case string
/// (`"internal" | "midi_in" | "ableton_link"`); the UI keys off the
/// same set.
pub fn state_changed_notification(
    state: &EngineState,
    last_event_id: u64,
    master_limiter_gain_reduction_db: f32,
    clock_source: ClockSource,
) -> RpcNotification {
    RpcNotification::new(
        method::NOTIFY_STATE_CHANGED,
        serde_json::json!({
            "state": state,
            "last_event_id": last_event_id,
            "master_limiter_gain_reduction_db": master_limiter_gain_reduction_db,
            "clock_source": clock_source.as_str(),
        }),
    )
}

/// Build an `audio_alert` notification frame.
pub fn audio_alert_notification(kind: &str, details: &str) -> RpcNotification {
    RpcNotification::new(
        method::NOTIFY_AUDIO_ALERT,
        serde_json::json!({ "kind": kind, "details": details }),
    )
}

/// Build an `engine.decode_error` notification frame.
///
/// `category` is the coarse failure class (`file_not_found`,
/// `format_unsupported`, `decoder_error`, …) used by the UI for icon /
/// copy selection. `error` is the human-readable stringification of the
/// underlying `DecodeError` and is shown in the toast body.
pub fn decode_error_notification(
    deck: DeckId,
    track_id: &str,
    category: &str,
    error: &str,
) -> RpcNotification {
    RpcNotification::new(
        method::NOTIFY_DECODE_ERROR,
        serde_json::json!({
            "deck": deck,
            "track_id": track_id,
            "category": category,
            "error": error,
        }),
    )
}

#[cfg(test)]
// The async dispatch tests hold `std::sync::MutexGuard` across `.await`
// points (the shared env-var lock). Under the default `tokio::test`
// `current_thread` runtime that's safe; relax the lint module-wide.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::state::{DeckId, EventKind};
    use crossbeam::channel;

    fn engine() -> EngineHandle {
        EngineHandle::new()
    }

    fn engine_with_sink() -> (EngineHandle, channel::Receiver<Event>) {
        let (tx, rx) = channel::unbounded::<Event>();
        (EngineHandle::with_event_sink(tx), rx)
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
    fn submit_event_accepts_deck_play_when_sink_wired() {
        let (e, rx) = engine_with_sink();
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "kind": { "DeckPlay": { "deck": "A" } } }),
            1,
        );
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["accepted"], Value::Bool(true));
        // The event was forwarded onto the channel.
        let ev = rx.try_recv().expect("event forwarded onto sink");
        match ev.kind {
            EventKind::DeckPlay { deck: DeckId::A } => {}
            other => panic!("unexpected event kind: {other:?}"),
        }
    }

    #[test]
    fn submit_event_no_sink_returns_engine_sink_unwired() {
        let e = engine();
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "kind": { "DeckPlay": { "deck": "A" } } }),
            1,
        );
        let resp = dispatch(&e, req);
        let err = resp.error.expect("expected error when sink is unwired");
        assert_eq!(err.code, super::super::error::ENGINE_SINK_UNWIRED);
        assert!(err.message.contains("engine sink not wired"));
    }

    #[test]
    fn submit_event_full_channel_returns_engine_offline() {
        // Bounded channel of capacity 1: fill it, then ensure the second
        // submit_event RPC trips the `-32000 engine offline` path.
        let (tx, _rx) = channel::bounded::<Event>(1);
        let e = EngineHandle::with_event_sink(tx);
        // First submit fills the channel (we never drain it).
        let r1 = dispatch(
            &e,
            submit(
                method::SUBMIT_EVENT,
                serde_json::json!({ "kind": { "DeckPlay": { "deck": "A" } } }),
                10,
            ),
        );
        assert!(r1.error.is_none(), "first submit should succeed: {r1:?}");
        // Second submit must error with engine_offline.
        let r2 = dispatch(
            &e,
            submit(
                method::SUBMIT_EVENT,
                serde_json::json!({ "kind": { "DeckPlay": { "deck": "B" } } }),
                11,
            ),
        );
        let err = r2.error.expect("expected -32000 on full channel");
        assert_eq!(err.code, super::super::error::ENGINE_OFFLINE);
        assert!(err.message.contains("engine offline"));
    }

    #[test]
    fn submit_event_disconnected_channel_returns_engine_offline() {
        // Drop the receiver; the sender's try_send must surface Disconnected.
        let (tx, rx) = channel::unbounded::<Event>();
        drop(rx);
        let e = EngineHandle::with_event_sink(tx);
        let resp = dispatch(
            &e,
            submit(
                method::SUBMIT_EVENT,
                serde_json::json!({ "kind": { "DeckPlay": { "deck": "A" } } }),
                12,
            ),
        );
        let err = resp.error.expect("expected -32000 on disconnected channel");
        assert_eq!(err.code, super::super::error::ENGINE_OFFLINE);
    }

    #[test]
    fn submit_event_unknown_kind_returns_invalid_params() {
        let (e, _rx) = engine_with_sink();
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
                master_limiter_gain_reduction_db,
                clock_source,
            } => {
                assert_eq!(last_event_id, 1);
                assert!(state.deck_a.playing);
                // No audio thread wired in tests; GR defaults to 0.
                assert!(master_limiter_gain_reduction_db.abs() < f32::EPSILON);
                // No shared clock wired in tests; source defaults to Internal.
                assert_eq!(clock_source, ClockSource::Internal);
            }
            BridgeNotice::AudioAlert { .. } | BridgeNotice::DecodeError { .. } => {
                panic!("expected StateChanged")
            }
        }
    }

    #[test]
    fn publish_decode_error_queues_bridge_notice_with_payload() {
        // Direct method test: publishing a decode error yields a
        // BridgeNotice::DecodeError variant readable from a subscriber.
        let e = engine();
        let mut rx = e.subscribe();
        e.publish_decode_error(
            DeckId::A,
            "track-42",
            "file_not_found",
            "io error opening /nope.mp3",
        );
        let n = rx.try_recv().expect("decode error notice queued");
        match n {
            BridgeNotice::DecodeError {
                deck,
                track_id,
                category,
                error,
            } => {
                assert_eq!(deck, DeckId::A);
                assert_eq!(track_id, "track-42");
                assert_eq!(category, "file_not_found");
                assert!(error.contains("/nope.mp3"));
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    #[test]
    fn decode_error_notification_wire_frame_round_trips() {
        // Wire-frame builder produces the exact JSON the protocol doc
        // promises: method=engine.decode_error, params={deck,
        // track_id, category, error}.
        let frame =
            decode_error_notification(DeckId::B, "abc-123", "format_unsupported", "bad header");
        let payload = serde_json::to_value(&frame).unwrap();
        assert_eq!(payload["jsonrpc"], "2.0");
        assert_eq!(payload["method"], "engine.decode_error");
        assert_eq!(payload["params"]["deck"], "B");
        assert_eq!(payload["params"]["track_id"], "abc-123");
        assert_eq!(payload["params"]["category"], "format_unsupported");
        assert_eq!(payload["params"]["error"], "bad header");
    }

    #[test]
    fn publish_decode_error_without_subscribers_does_not_panic() {
        // Broadcast send returns Err when nobody is listening; the
        // publish helper swallows that so a quiet client list never
        // takes down the control loop.
        let e = engine();
        // No subscribe() call before publish — exercises the drop path.
        e.publish_decode_error(DeckId::A, "t", "decoder_error", "details");
    }

    #[test]
    fn state_changed_notification_includes_master_limiter_gr_when_wired() {
        // Wire a synthetic GR atomic (= -3.0 dB stored as -30 i16) and
        // verify the bridge stamps the decoded dB onto every outgoing
        // StateChanged. Exercises the audio→bridge side-channel
        // without needing a real audio thread.
        let e = engine();
        let gr = Arc::new(AtomicI16::new(-30));
        e.attach_master_limiter_gr(Arc::clone(&gr));
        let mut rx = e.subscribe();
        e.submit_event_kind(EventKind::DeckPlay { deck: DeckId::A }, EventSource::Ui);
        let n = rx.try_recv().expect("notification queued");
        match n {
            BridgeNotice::StateChanged {
                master_limiter_gain_reduction_db,
                ..
            } => {
                assert!(
                    (master_limiter_gain_reduction_db - (-3.0)).abs() < 0.05,
                    "expected -3 dB GR, got {master_limiter_gain_reduction_db}",
                );
            }
            BridgeNotice::AudioAlert { .. } | BridgeNotice::DecodeError { .. } => {
                panic!("expected StateChanged")
            }
        }
        // And the wire-frame builder mirrors the same value.
        let frame =
            state_changed_notification(&EngineState::default(), 1, -3.0, ClockSource::Internal);
        let payload = serde_json::to_value(&frame).unwrap();
        let gr_v = payload["params"]["master_limiter_gain_reduction_db"]
            .as_f64()
            .unwrap();
        assert!((gr_v - (-3.0)).abs() < 1e-6);
    }

    #[test]
    fn state_changed_notification_carries_clock_source_when_wired() {
        // Wire a SharedClock, flip it to MidiIn, and verify both the
        // `BridgeNotice` and the wire-frame builder surface the
        // kebab-case source label. This is the load-bearing field for
        // the UI BPM-lock badge — without it the badge can't tell when
        // an external master is providing tempo.
        let e = engine();
        let clock = SharedClock::new();
        clock.set_clock_source(ClockSource::MidiIn);
        e.attach_shared_clock(clock.clone());
        let mut rx = e.subscribe();
        e.submit_event_kind(EventKind::DeckPlay { deck: DeckId::A }, EventSource::Ui);
        let n = rx.try_recv().expect("notification queued");
        match n {
            BridgeNotice::StateChanged { clock_source, .. } => {
                assert_eq!(clock_source, ClockSource::MidiIn);
            }
            BridgeNotice::AudioAlert { .. } | BridgeNotice::DecodeError { .. } => {
                panic!("expected StateChanged")
            }
        }
        // Wire frame must serialize the kebab-case label, NOT a numeric
        // discriminant — the UI's `ClockSourceLabel` is a string union.
        let frame =
            state_changed_notification(&EngineState::default(), 1, 0.0, ClockSource::MidiIn);
        let payload = serde_json::to_value(&frame).unwrap();
        assert_eq!(payload["params"]["clock_source"], "midi_in");
        let frame2 =
            state_changed_notification(&EngineState::default(), 1, 0.0, ClockSource::AbletonLink);
        let payload2 = serde_json::to_value(&frame2).unwrap();
        assert_eq!(payload2["params"]["clock_source"], "ableton_link");
        let frame3 =
            state_changed_notification(&EngineState::default(), 1, 0.0, ClockSource::Internal);
        let payload3 = serde_json::to_value(&frame3).unwrap();
        assert_eq!(payload3["params"]["clock_source"], "internal");
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
    fn submit_event_accepts_bare_kind_shape_with_sink() {
        let (e, rx) = engine_with_sink();
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "DeckPlay": { "deck": "A" } }),
            8,
        );
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let ev = rx.try_recv().expect("event forwarded onto sink");
        match ev.kind {
            EventKind::DeckPlay { deck: DeckId::A } => {}
            other => panic!("unexpected event kind: {other:?}"),
        }
        // Source defaults to Ui when omitted from the bare shape.
        assert!(matches!(ev.source, EventSource::Ui));
    }

    #[test]
    fn forward_event_returns_engine_offline_on_full_bounded_channel() {
        // Direct method test for the helper.
        let (tx, _rx) = channel::bounded::<Event>(1);
        let e = EngineHandle::with_event_sink(tx);
        let ev = Event {
            id: 1,
            ts_micros: 0,
            source: EventSource::Ui,
            kind: EventKind::DeckPlay { deck: DeckId::A },
        };
        // Fill the channel.
        e.forward_event(ev.clone()).expect("first send fits");
        // Next send must return engine_offline.
        let err = e.forward_event(ev).expect_err("expected engine_offline");
        assert_eq!(err.code, super::super::error::ENGINE_OFFLINE);
    }

    #[test]
    fn has_event_sink_reflects_constructor_choice() {
        assert!(!EngineHandle::new().has_event_sink());
        let (tx, _rx) = channel::unbounded::<Event>();
        assert!(EngineHandle::with_event_sink(tx).has_event_sink());
    }

    // ---------------------------------------------------------------
    // auth.hello + pending-auth gate.
    // ---------------------------------------------------------------

    #[test]
    fn auth_hello_with_valid_token_authenticates() {
        let e = engine();
        let auth = AuthConfig::with_token("s3cret");
        let req = submit(
            method::AUTH_HELLO,
            serde_json::json!({ "token": "s3cret" }),
            10,
        );
        let (resp, new_state) = dispatch_with_auth(&e, &auth, AuthState::PendingAuth, req);
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["authed"], Value::Bool(true));
        assert!(result.get("session").is_some());
        assert_eq!(new_state, AuthState::Authed);
    }

    #[test]
    fn auth_hello_with_invalid_token_rejects_and_keeps_pending() {
        let e = engine();
        let auth = AuthConfig::with_token("s3cret");
        let req = submit(
            method::AUTH_HELLO,
            serde_json::json!({ "token": "wrong" }),
            11,
        );
        let (resp, new_state) = dispatch_with_auth(&e, &auth, AuthState::PendingAuth, req);
        let err = resp.error.expect("expected error");
        assert_eq!(err.code, super::super::error::AUTH_REJECTED);
        assert_eq!(new_state, AuthState::PendingAuth);
    }

    #[test]
    fn auth_hello_is_idempotent_when_already_authed() {
        let e = engine();
        let auth = AuthConfig::with_token("s3cret");
        let req = submit(
            method::AUTH_HELLO,
            serde_json::json!({ "token": "s3cret" }),
            12,
        );
        let (resp, new_state) = dispatch_with_auth(&e, &auth, AuthState::Authed, req);
        assert!(resp.error.is_none(), "{:?}", resp.error);
        assert_eq!(resp.result.unwrap()["authed"], Value::Bool(true));
        assert_eq!(new_state, AuthState::Authed);
    }

    #[test]
    fn pending_auth_gate_blocks_submit_event_with_auth_rejected() {
        let e = engine();
        let auth = AuthConfig::with_token("s3cret");
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "kind": { "DeckPlay": { "deck": "A" } } }),
            13,
        );
        let (resp, new_state) = dispatch_with_auth(&e, &auth, AuthState::PendingAuth, req);
        let err = resp.error.expect("expected auth_rejected");
        assert_eq!(err.code, super::super::error::AUTH_REJECTED);
        assert_eq!(new_state, AuthState::PendingAuth);
        // Engine state must be untouched — gate is before the reducer.
        assert!(!e.snapshot().deck_a.playing);
    }

    #[test]
    fn authed_state_lets_submit_event_through() {
        // Once the connection is Authed, `submit_event` must reach the
        // forward_event path. With a wired sink, that surfaces as
        // `{accepted: true}` and the event landing on the receiver.
        let (e, rx) = engine_with_sink();
        let auth = AuthConfig::with_token("s3cret");
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "kind": { "DeckPlay": { "deck": "A" } } }),
            14,
        );
        let (resp, new_state) = dispatch_with_auth(&e, &auth, AuthState::Authed, req);
        assert!(resp.error.is_none(), "{:?}", resp.error);
        assert_eq!(resp.result.unwrap()["accepted"], Value::Bool(true));
        assert_eq!(new_state, AuthState::Authed);
        let ev = rx.try_recv().expect("event forwarded onto sink");
        match ev.kind {
            EventKind::DeckPlay { deck: DeckId::A } => {}
            other => panic!("unexpected event kind: {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // engine.list_effects — manifest from the audio effects registry.
    // ---------------------------------------------------------------

    #[test]
    fn list_effects_returns_all_four_built_in_effects() {
        let e = engine();
        let req = submit(method::LIST_EFFECTS, Value::Null, 30);
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.expect("list_effects must return a result");
        let effects = result
            .get("effects")
            .and_then(Value::as_array)
            .expect("`effects` array must be present in result");
        assert_eq!(
            effects.len(),
            4,
            "expected 4 effects (filter, echo, reverb, gate), got {}",
            effects.len()
        );

        // Each entry must have id, name, params; params descriptors must
        // expose name + min + max + default.
        let filter = &effects[0];
        assert_eq!(filter["id"], serde_json::json!(1));
        assert_eq!(filter["name"], serde_json::json!("filter"));
        let filter_params = filter["params"]
            .as_array()
            .expect("filter.params must be an array");
        assert_eq!(filter_params.len(), 3);
        assert_eq!(filter_params[0]["name"], serde_json::json!("cutoff_hz"));
        assert_eq!(filter_params[0]["min"], serde_json::json!(20.0));
        assert_eq!(filter_params[0]["max"], serde_json::json!(20_000.0));
        assert_eq!(filter_params[0]["default"], serde_json::json!(500.0));
        assert_eq!(filter_params[1]["name"], serde_json::json!("resonance"));
        assert_eq!(filter_params[2]["name"], serde_json::json!("mode"));

        let echo = &effects[1];
        assert_eq!(echo["id"], serde_json::json!(2));
        assert_eq!(echo["name"], serde_json::json!("echo"));
        let echo_params = echo["params"].as_array().unwrap();
        assert_eq!(echo_params.len(), 3);
        assert_eq!(echo_params[0]["name"], serde_json::json!("time_ms"));
        assert_eq!(echo_params[1]["name"], serde_json::json!("feedback"));
        assert_eq!(echo_params[2]["name"], serde_json::json!("tone"));

        let reverb = &effects[2];
        assert_eq!(reverb["id"], serde_json::json!(3));
        assert_eq!(reverb["name"], serde_json::json!("reverb"));
        let reverb_params = reverb["params"].as_array().unwrap();
        assert_eq!(reverb_params.len(), 3);
        assert_eq!(reverb_params[0]["name"], serde_json::json!("room_size"));
        assert_eq!(reverb_params[1]["name"], serde_json::json!("damping"));
        assert_eq!(reverb_params[2]["name"], serde_json::json!("width"));

        let gate = &effects[3];
        assert_eq!(gate["id"], serde_json::json!(4));
        assert_eq!(gate["name"], serde_json::json!("gate"));
        let gate_params = gate["params"].as_array().unwrap();
        assert_eq!(gate_params.len(), 2);
        assert_eq!(gate_params[0]["name"], serde_json::json!("period_div"));
        assert_eq!(gate_params[1]["name"], serde_json::json!("duty"));
    }

    #[test]
    fn list_effects_is_callable_without_event_sink() {
        // Bare engine (no wired sink) — list_effects is read-only so it
        // must still succeed; the UI calls it on connect before any
        // submit_event is in flight.
        let e = engine();
        let req = submit(method::LIST_EFFECTS, Value::Null, 31);
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    }

    #[test]
    fn auth_hello_missing_params_is_invalid_params() {
        let e = engine();
        let auth = AuthConfig::with_token("s3cret");
        let req = RpcRequest {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method::AUTH_HELLO.into(),
            params: None,
            id: Some(RpcId::Num(15)),
        };
        let (resp, new_state) = dispatch_with_auth(&e, &auth, AuthState::PendingAuth, req);
        assert_eq!(
            resp.error.unwrap().code,
            super::super::error::INVALID_PARAMS
        );
        assert_eq!(new_state, AuthState::PendingAuth);
    }

    // ---------------------------------------------------------------
    // engine.list_sessions + engine.replay_session — history surface.
    //
    // These tests share process env (`HYPEHOUSE_EVENT_LOG_DIR`) with
    // the persistence::sessions tests, so we re-use the same one-shot
    // mutex approach: serialize anything that mutates the env to keep
    // cargo's parallel runner happy.
    // ---------------------------------------------------------------

    fn sessions_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn sessions_scratch_root(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("hh-rpc-sessions-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("scratch dir create");
        dir
    }

    #[test]
    fn list_sessions_rpc_returns_empty_envelope_for_empty_root() {
        let _g = sessions_test_lock();
        let root = sessions_scratch_root("rpc-empty");
        std::env::set_var(super::super::super::persistence::ENV_LOG_DIR, &root);
        let e = engine();
        let req = submit(method::LIST_SESSIONS, Value::Null, 40);
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.expect("must have result");
        let sessions = result
            .get("sessions")
            .and_then(Value::as_array)
            .expect("sessions array");
        assert!(sessions.is_empty(), "expected empty list, got {sessions:?}");
        std::env::remove_var(super::super::super::persistence::ENV_LOG_DIR);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_sessions_rpc_surfaces_session_summary_fields() {
        let _g = sessions_test_lock();
        let root = sessions_scratch_root("rpc-fields");
        std::env::set_var(super::super::super::persistence::ENV_LOG_DIR, &root);

        // Seed one session with a couple of events + master.wav.
        let sid = "20260301T010101Z-aaaa";
        let dir = root.join(sid);
        std::fs::create_dir_all(&dir).expect("dir");
        let e1 = Event {
            id: 1,
            ts_micros: 1_000_000,
            source: EventSource::Ui,
            kind: EventKind::SessionStart,
        };
        let e2 = Event {
            id: 2,
            ts_micros: 2_000_000,
            source: EventSource::Ui,
            kind: EventKind::DeckPlay { deck: DeckId::A },
        };
        let mut f = std::fs::File::create(dir.join("events.jsonl")).expect("create events");
        use std::io::Write as _;
        writeln!(f, "{}", serde_json::to_string(&e1).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&e2).unwrap()).unwrap();
        drop(f);
        std::fs::write(dir.join("master.wav"), b"RIFFfakeWAVE").expect("write wav");

        let e = engine();
        let req = submit(method::LIST_SESSIONS, Value::Null, 41);
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let result = resp.result.expect("result");
        let sessions = result.get("sessions").and_then(Value::as_array).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s["id"], serde_json::json!(sid));
        assert_eq!(s["event_count"], serde_json::json!(2));
        assert_eq!(s["started_at_micros"], serde_json::json!(1_000_000));
        assert_eq!(s["ended_at_micros"], serde_json::json!(2_000_000));
        assert_eq!(s["has_recording"], serde_json::json!(true));
        // master.wav size is a positive number.
        let size = s["recording_size_bytes"].as_u64().expect("size present");
        assert!(size > 0);

        std::env::remove_var(super::super::super::persistence::ENV_LOG_DIR);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn replay_session_rpc_returns_reconstructed_state() {
        let _g = sessions_test_lock();
        let root = sessions_scratch_root("rpc-replay");
        std::env::set_var(super::super::super::persistence::ENV_LOG_DIR, &root);
        let sid = "20260301T020202Z-bbbb";
        let dir = root.join(sid);
        std::fs::create_dir_all(&dir).expect("dir");
        let events = [
            Event {
                id: 1,
                ts_micros: 1,
                source: EventSource::Ui,
                kind: EventKind::SessionStart,
            },
            Event {
                id: 2,
                ts_micros: 2,
                source: EventSource::Ui,
                kind: EventKind::Crossfader { value: 0.25 },
            },
        ];
        let mut f = std::fs::File::create(dir.join("events.jsonl")).expect("create events");
        use std::io::Write as _;
        for e in &events {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
        }
        drop(f);

        let e = engine();
        let req = submit(
            method::REPLAY_SESSION,
            serde_json::json!({ "session_id": sid }),
            42,
        );
        let resp = dispatch(&e, req);
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let result = resp.result.expect("result");
        assert_eq!(result["event_count"], serde_json::json!(2));
        let state = &result["state"];
        assert_eq!(state["session_active"], serde_json::json!(true));
        let crossfader = state["crossfader"].as_f64().unwrap();
        assert!((crossfader - 0.25).abs() < 1e-6);

        std::env::remove_var(super::super::super::persistence::ENV_LOG_DIR);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn replay_session_rpc_rejects_missing_session_id_param() {
        let _g = sessions_test_lock();
        let root = sessions_scratch_root("rpc-missing-id");
        std::env::set_var(super::super::super::persistence::ENV_LOG_DIR, &root);
        let e = engine();
        // Missing params entirely.
        let req = RpcRequest {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method::REPLAY_SESSION.into(),
            params: None,
            id: Some(RpcId::Num(43)),
        };
        let resp = dispatch(&e, req);
        assert_eq!(
            resp.error.unwrap().code,
            super::super::error::INVALID_PARAMS
        );
        // Wrong shape — missing session_id field.
        let req = submit(method::REPLAY_SESSION, serde_json::json!({}), 44);
        let resp = dispatch(&e, req);
        assert_eq!(
            resp.error.unwrap().code,
            super::super::error::INVALID_PARAMS
        );
        std::env::remove_var(super::super::super::persistence::ENV_LOG_DIR);
        std::fs::remove_dir_all(&root).ok();
    }

    // ---------------------------------------------------------------
    // library.* proxy routing via dispatch_with_auth_async.
    //
    // These tests cover the integration between the bridge dispatch and
    // the library_proxy module. They share process env
    // (`HYPEHOUSE_COPILOT_URL`) with the library_proxy unit tests, so we
    // serialize the env mutations through the same lock pattern.
    // ---------------------------------------------------------------

    fn proxy_env_lock() -> std::sync::MutexGuard<'static, ()> {
        // Shared with `library_proxy::tests` — the env var is process
        // global so both modules must serialize through one Mutex.
        super::super::library_proxy::copilot_env_lock()
    }

    #[tokio::test]
    async fn dispatch_async_routes_library_methods_to_proxy() {
        // With proxy disabled, every `library.*` call must surface
        // `-32000 engine offline` — proving the dispatch routed to the
        // proxy rather than the engine handle (which would have
        // returned `method_not_found` for `library.list_tracks`).
        let _g = proxy_env_lock();
        std::env::set_var(super::super::library_proxy::ENV_COPILOT_URL, "");
        let e = engine();
        let auth = AuthConfig::with_token("s3cret");
        let req = submit(
            "library.list_tracks",
            serde_json::json!({ "limit": 100 }),
            50,
        );
        let (resp, new_state) = dispatch_with_auth_async(&e, &auth, AuthState::Authed, req).await;
        std::env::remove_var(super::super::library_proxy::ENV_COPILOT_URL);
        assert_eq!(new_state, AuthState::Authed);
        let err = resp.error.expect("proxy disabled must error");
        assert_eq!(err.code, super::super::error::ENGINE_OFFLINE);
    }

    #[tokio::test]
    async fn dispatch_async_passes_non_library_methods_to_local_engine() {
        // `engine.submit_event` must still dispatch in-process — the
        // proxy hop is library-only. Engine ack confirms the local
        // path ran. With proxy disabled we also prove the local path
        // is not accidentally short-circuited by the routing rule.
        let _g = proxy_env_lock();
        std::env::set_var(super::super::library_proxy::ENV_COPILOT_URL, "");
        let (e, rx) = engine_with_sink();
        let auth = AuthConfig::with_token("s3cret");
        let req = submit(
            method::SUBMIT_EVENT,
            serde_json::json!({ "kind": { "DeckPlay": { "deck": "A" } } }),
            51,
        );
        let (resp, new_state) = dispatch_with_auth_async(&e, &auth, AuthState::Authed, req).await;
        std::env::remove_var(super::super::library_proxy::ENV_COPILOT_URL);
        assert_eq!(new_state, AuthState::Authed);
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["accepted"], Value::Bool(true));
        let ev = rx.try_recv().expect("event forwarded onto local sink");
        match ev.kind {
            EventKind::DeckPlay { deck: DeckId::A } => {}
            other => panic!("unexpected event kind: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_async_enforces_auth_gate_before_proxy_hop() {
        // Pending-auth connections must NOT trigger an outbound proxy
        // call — the gate fires first. We assert by setting the env to
        // a URL that would otherwise reach a listener; the test passes
        // because the dispatch short-circuits with auth_rejected and
        // we never touch the network.
        let _g = proxy_env_lock();
        std::env::set_var(
            super::super::library_proxy::ENV_COPILOT_URL,
            "http://127.0.0.1:1/rpc", // would refuse on connect
        );
        let e = engine();
        let auth = AuthConfig::with_token("s3cret");
        let req = submit("library.list_tracks", serde_json::json!({}), 52);
        let (resp, new_state) =
            dispatch_with_auth_async(&e, &auth, AuthState::PendingAuth, req).await;
        std::env::remove_var(super::super::library_proxy::ENV_COPILOT_URL);
        let err = resp.error.expect("expected auth_rejected");
        assert_eq!(err.code, super::super::error::AUTH_REJECTED);
        assert_eq!(new_state, AuthState::PendingAuth);
    }

    #[tokio::test]
    async fn dispatch_async_routes_all_library_methods_uniformly() {
        // Every `library.*` method must traverse the proxy — not just
        // `library.list_tracks`. We sample the full method surface
        // (per `copilot/library_rpc.py::LibraryRpcHandler::METHODS`)
        // and assert each one comes back with the proxy-disabled
        // error envelope.
        let _g = proxy_env_lock();
        std::env::set_var(super::super::library_proxy::ENV_COPILOT_URL, "");
        let e = engine();
        let auth = AuthConfig::with_token("s3cret");
        for (method_name, params) in [
            ("library.list_tracks", serde_json::json!({})),
            ("library.search_tracks", serde_json::json!({ "query": "" })),
            ("library.add_track", serde_json::json!({ "path": "/x" })),
            (
                "library.add_track_from_directory",
                serde_json::json!({ "path": "/x" }),
            ),
            (
                "library.set_hot_cues",
                serde_json::json!({
                    "track_id": "x",
                    "hot_cues": [null, null, null, null, null, null, null, null]
                }),
            ),
            (
                "library.get_waveform",
                serde_json::json!({ "track_id": "x" }),
            ),
        ] {
            let req = submit(method_name, params, 60);
            let (resp, _new_state) =
                dispatch_with_auth_async(&e, &auth, AuthState::Authed, req).await;
            let err = resp
                .error
                .unwrap_or_else(|| panic!("expected error for {method_name}"));
            assert_eq!(
                err.code,
                super::super::error::ENGINE_OFFLINE,
                "method {method_name} did not route to proxy (got code {})",
                err.code
            );
        }
        std::env::remove_var(super::super::library_proxy::ENV_COPILOT_URL);
    }

    #[test]
    fn replay_session_rpc_rejects_path_traversal_id() {
        let _g = sessions_test_lock();
        let root = sessions_scratch_root("rpc-traversal");
        std::env::set_var(super::super::super::persistence::ENV_LOG_DIR, &root);
        let e = engine();
        // Forward slash — caught by the separator check.
        let req = submit(
            method::REPLAY_SESSION,
            serde_json::json!({ "session_id": "foo/bar" }),
            45,
        );
        let resp = dispatch(&e, req);
        let err = resp.error.expect("expected error");
        assert_eq!(err.code, super::super::error::INVALID_PARAMS);
        assert!(
            err.data
                .as_ref()
                .and_then(Value::as_str)
                .map(|s| s.contains("path separator"))
                .unwrap_or(false),
            "expected path separator error: {err:?}"
        );
        // Leading dot — covers `..` parent-dir traversal.
        let req = submit(
            method::REPLAY_SESSION,
            serde_json::json!({ "session_id": "../etc" }),
            46,
        );
        let resp = dispatch(&e, req);
        let err = resp.error.expect("expected error");
        assert_eq!(err.code, super::super::error::INVALID_PARAMS);
        assert!(
            err.data
                .as_ref()
                .and_then(Value::as_str)
                .map(|s| s.contains("starts with '.'"))
                .unwrap_or(false),
            "expected leading-dot error: {err:?}"
        );
        std::env::remove_var(super::super::super::persistence::ENV_LOG_DIR);
        std::fs::remove_dir_all(&root).ok();
    }
}
