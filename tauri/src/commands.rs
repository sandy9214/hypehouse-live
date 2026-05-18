//! Tauri commands exposed to the UI via `invoke()`.
//!
//! These are the *only* sanctioned channel from the webview into the
//! desktop process. We intentionally keep the surface small:
//!
//!   * `get_bridge_url` — where to point the JSON-RPC WebSocket client.
//!   * `get_bridge_token` — per-launch bearer token for `auth.hello`.
//!
//! The UI calls both at startup, then uses them to construct the same
//! `JsonRpcWS` it already uses in browser-only mode. No other knobs.

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
}
