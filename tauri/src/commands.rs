//! Tauri commands exposed to the UI via `invoke()`.
//!
//! These are the *only* sanctioned channel from the webview into the
//! desktop process. We intentionally keep the surface small:
//!
//!   * `get_bridge_url` — where to point the JSON-RPC WebSocket client.
//!   * `get_bridge_token` — per-launch bearer token for `auth.hello`.
//!   * `check_for_updates` — force an updater poll (ADR-008).
//!   * `install_pending_update` — apply a downloaded update + restart.
//!
//! The UI calls both at startup, then uses them to construct the same
//! `JsonRpcWS` it already uses in browser-only mode. No other knobs.

use crate::updater::{CheckResult, HttpUpdater, Updater, UpdaterConfig, UpdaterError};
use rand::RngCore;
use serde::Serialize;
use std::sync::OnceLock;

/// Default bridge address. Matches `engine/src/bridge/config.rs` and
/// `ui/vite.config.ts` (`/ws` proxy target).
const DEFAULT_BRIDGE_URL: &str = "ws://127.0.0.1:8765";

/// Number of random bytes used for the session token. 32 bytes = 256
/// bits, which exceeds the bearer-token entropy guidance in RFC 6750.
const TOKEN_BYTES: usize = 32;

/// Hex-encoded length of a token. Verified by unit tests so the UI
/// side can assume a stable shape.
pub const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// Lazy-init singleton — same token returned to the UI and passed to
/// the engine via env var during a single app launch.
static SESSION_TOKEN: OnceLock<String> = OnceLock::new();

/// Generate (or return cached) the per-launch session token.
///
/// Public because `main.rs` needs to inject it into the engine's
/// environment *before* the webview opens.
pub fn session_token() -> &'static str {
    SESSION_TOKEN.get_or_init(generate_token)
}

/// Produce a fresh hex-encoded token. Pulled out so unit tests can
/// assert randomness without poisoning the global cache.
pub fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Wrapper struct so the JSON returned to the UI has stable shape if
/// we ever want to add fields (e.g. a token expiry).
#[derive(Serialize)]
pub struct BridgeToken {
    pub token: String,
}

#[tauri::command]
pub fn get_bridge_url() -> String {
    // For v0.1 we always return the loopback default; later PRs can
    // wire a config file or CLI flag. The UI MUST go through this
    // command rather than hardcoding so the contract stays in one
    // place.
    std::env::var("HYPEHOUSE_BRIDGE_URL").unwrap_or_else(|_| DEFAULT_BRIDGE_URL.to_string())
}

#[tauri::command]
pub fn get_bridge_token() -> String {
    session_token().to_string()
}

/// JSON-shaped result returned to the UI from `check_for_updates`. We
/// flatten the `CheckResult` so the UI doesn't need to crack open an
/// `Option<String>` on the happy path.
#[derive(Serialize)]
pub struct CheckForUpdatesResponse {
    pub available: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

impl From<Result<CheckResult, UpdaterError>> for CheckForUpdatesResponse {
    fn from(value: Result<CheckResult, UpdaterError>) -> Self {
        match value {
            Ok(c) => Self {
                available: c.available,
                version: c.version,
                error: None,
            },
            Err(e) => Self {
                available: false,
                version: None,
                error: Some(e.to_string()),
            },
        }
    }
}

/// Run a one-shot updater poll using whatever `Updater` impl the
/// caller passes. Split out from the `#[tauri::command]` shim so unit
/// tests can substitute a fake.
pub fn check_for_updates_with<U: Updater>(
    updater: &U,
    current_version: &str,
) -> CheckForUpdatesResponse {
    updater.check(current_version).into()
}

/// Tauri command — force an updater check. Returns the same JSON shape
/// regardless of success/failure so the UI has one parser. The default
/// config is loaded from `tauri.conf.json` via the updater plugin once
/// the integration PR lands; for now we construct an `HttpUpdater`
/// with defaults pulled from the environment.
#[tauri::command]
pub fn check_for_updates() -> CheckForUpdatesResponse {
    let cfg = UpdaterConfig {
        // Disabled-by-default keeps the scaffold safe: the command
        // resolves cleanly without making bogus network calls. Real
        // wiring flips this from a user-settings panel.
        opt_in: false,
        ..UpdaterConfig::default()
    };
    let updater = HttpUpdater::new(cfg);
    check_for_updates_with(&updater, env!("CARGO_PKG_VERSION"))
}

/// JSON-shaped result for `install_pending_update`. Today the scaffold
/// returns `installed: false` + a placeholder reason; the integration
/// PR replaces the body with a real `app_handle.updater().install()`
/// + `app_handle.restart()` call.
#[derive(Serialize)]
pub struct InstallPendingUpdateResponse {
    pub installed: bool,
    pub reason: String,
}

#[tauri::command]
pub fn install_pending_update() -> InstallPendingUpdateResponse {
    // PLACEHOLDER — wiring tauri-plugin-updater::install() lands in
    // the follow-up PR. Returning a structured no-op keeps the IPC
    // contract stable so the UI can already write its restart flow.
    InstallPendingUpdateResponse {
        installed: false,
        reason: "scaffold — install + restart wiring deferred to ADR-008 follow-up PR".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generate_token_returns_unique_hex_values() {
        // The OnceLock-cached `session_token` is exercised separately;
        // this test pokes the raw generator so we can prove that two
        // back-to-back calls don't collide.
        let mut seen: HashSet<String> = HashSet::with_capacity(8);
        for _ in 0..8 {
            let t = generate_token();
            assert_eq!(
                t.len(),
                TOKEN_HEX_LEN,
                "token must be {TOKEN_HEX_LEN} hex chars"
            );
            assert!(
                t.chars().all(|c| c.is_ascii_hexdigit()),
                "token contains non-hex byte: {t}"
            );
            assert!(seen.insert(t.clone()), "duplicate token: {t}");
        }
    }

    #[test]
    fn generate_token_yields_at_least_32_raw_bytes() {
        let t = generate_token();
        let decoded = hex::decode(&t).expect("token is valid hex");
        assert!(
            decoded.len() >= 32,
            "token must decode to ≥ 32 random bytes, got {}",
            decoded.len()
        );
    }

    #[test]
    fn get_bridge_url_default_is_loopback_8765() {
        std::env::remove_var("HYPEHOUSE_BRIDGE_URL");
        assert_eq!(get_bridge_url(), DEFAULT_BRIDGE_URL);
    }

    #[test]
    fn get_bridge_url_honours_env_override() {
        std::env::set_var("HYPEHOUSE_BRIDGE_URL", "ws://example.test:9000");
        let url = get_bridge_url();
        std::env::remove_var("HYPEHOUSE_BRIDGE_URL");
        assert_eq!(url, "ws://example.test:9000");
    }

    #[test]
    fn session_token_is_stable_within_a_run() {
        let a = session_token().to_string();
        let b = session_token().to_string();
        assert_eq!(
            a, b,
            "session token must be stable across calls in a single run"
        );
        assert_eq!(a.len(), TOKEN_HEX_LEN);
    }

    #[test]
    fn get_bridge_token_returns_same_as_session_token() {
        // `get_bridge_token` wraps the singleton; the Tauri command
        // attribute doesn't change the function call semantics for
        // direct testing.
        let from_cmd = get_bridge_token();
        let from_helper = session_token();
        assert_eq!(from_cmd, from_helper);
    }

    // --- Updater command tests (ADR-008) ---------------------------------
    //
    // We assert against `check_for_updates_with` (the testable seam)
    // rather than the `#[tauri::command]` shim, because the latter
    // hardcodes opt_in=false to keep the scaffold safe.

    struct StubUpdater {
        result: Result<CheckResult, UpdaterError>,
    }

    impl Updater for StubUpdater {
        fn check(&self, _current_version: &str) -> Result<CheckResult, UpdaterError> {
            // Clone the canned result so the trait method can be
            // called repeatedly in a single test.
            match &self.result {
                Ok(c) => Ok(c.clone()),
                Err(UpdaterError::Disabled) => Err(UpdaterError::Disabled),
                Err(UpdaterError::Network(s)) => Err(UpdaterError::Network(s.clone())),
                Err(UpdaterError::BadManifest(s)) => Err(UpdaterError::BadManifest(s.clone())),
                Err(UpdaterError::BadVersion(s)) => Err(UpdaterError::BadVersion(s.clone())),
            }
        }
    }

    #[test]
    fn check_for_updates_happy_path_returns_available_true() {
        let stub = StubUpdater {
            result: Ok(CheckResult {
                available: true,
                version: Some("0.2.0".into()),
                notes: Some("hotfix".into()),
            }),
        };
        let resp = check_for_updates_with(&stub, "0.1.0");
        assert!(resp.available);
        assert_eq!(resp.version.as_deref(), Some("0.2.0"));
        assert!(resp.error.is_none());
    }

    #[test]
    fn check_for_updates_disabled_returns_structured_error() {
        let stub = StubUpdater {
            result: Err(UpdaterError::Disabled),
        };
        let resp = check_for_updates_with(&stub, "0.1.0");
        assert!(!resp.available);
        assert!(resp.version.is_none());
        assert!(resp.error.is_some());
        assert!(resp.error.unwrap().contains("opt_in"));
    }

    #[test]
    fn check_for_updates_tauri_command_is_safe_when_disabled() {
        // The plain `#[tauri::command]` shim should *never* panic and
        // should return a structured response even when no manifest
        // server exists. This guarantees the UI can always render.
        let resp = check_for_updates();
        assert!(!resp.available);
        // The scaffold default is opt_in=false, so error should mention it.
        assert!(resp.error.as_deref().unwrap_or("").contains("opt_in"));
    }

    #[test]
    fn install_pending_update_returns_structured_placeholder() {
        let resp = install_pending_update();
        // Scaffold contract: never panics, always returns installed=false,
        // always carries a non-empty reason explaining why.
        assert!(!resp.installed);
        assert!(!resp.reason.is_empty());
    }
}
