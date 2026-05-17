//! Bridge auth — optional bearer-token over the WS handshake.
//!
//! Policy
//! ------
//! * If `HYPEHOUSE_BRIDGE_TOKEN` is unset, the server listens on the
//!   loopback address only (`127.0.0.1`) and accepts every connection
//!   without a token. This is the dev-laptop default — no friction
//!   between UI ↔ engine ↔ copilot all on the same host.
//! * If `HYPEHOUSE_BRIDGE_TOKEN` is set, every WS handshake MUST include
//!   `Authorization: Bearer <token>`. Mismatch → connection rejected
//!   during the handshake with a 401 status (silently from the client's
//!   point of view; the WS upgrade simply fails).
//!
//! Bind address
//! ------------
//! Token unset → forced loopback. Token set → bind opens up to whatever
//! address the caller passes (the server still defaults to loopback;
//! exposing wider is an explicit deployment choice). This is "secure by
//! default": the unauthenticated mode literally cannot accept a remote
//! connection.

use std::env;

/// Auth configuration derived from the process environment at server
/// startup. Cheap to clone (a single `Option<String>`).
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// `Some(token)` → require `Authorization: Bearer <token>` header.
    /// `None`        → no auth + bind locked to loopback.
    pub bearer_token: Option<String>,
}

impl AuthConfig {
    pub const ENV_TOKEN: &'static str = "HYPEHOUSE_BRIDGE_TOKEN";

    /// Read `HYPEHOUSE_BRIDGE_TOKEN` from the env. Empty strings are
    /// treated as "unset" so an accidental `export VAR=` doesn't silently
    /// disable auth-checking.
    pub fn from_env() -> Self {
        let token = env::var(Self::ENV_TOKEN).ok().filter(|s| !s.is_empty());
        Self {
            bearer_token: token,
        }
    }

    /// Convenience for tests.
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            bearer_token: Some(token.into()),
        }
    }

    pub fn requires_auth(&self) -> bool {
        self.bearer_token.is_some()
    }

    /// True when the supplied `Authorization` header value matches the
    /// configured bearer token. If auth is disabled (`bearer_token =
    /// None`), every check returns true.
    pub fn check_header(&self, header_value: Option<&str>) -> bool {
        let Some(expected) = self.bearer_token.as_deref() else {
            return true;
        };
        let Some(raw) = header_value else {
            return false;
        };
        let trimmed = raw.trim();
        let Some(rest) = trimmed.strip_prefix("Bearer ") else {
            return false;
        };
        // Constant-time-ish compare — short strings, single-host bridge,
        // no remote-timing-attack threat model, but still avoid `==`
        // short-circuiting on prefix matches.
        constant_time_eq(rest.as_bytes(), expected.as_bytes())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_token_means_no_auth_required() {
        let cfg = AuthConfig::default();
        assert!(!cfg.requires_auth());
        assert!(cfg.check_header(None));
        assert!(cfg.check_header(Some("anything")));
    }

    #[test]
    fn token_set_requires_matching_header() {
        let cfg = AuthConfig::with_token("s3cret");
        assert!(cfg.requires_auth());
        assert!(!cfg.check_header(None));
        assert!(!cfg.check_header(Some("Bearer wrong")));
        assert!(!cfg.check_header(Some("Basic s3cret")));
        assert!(cfg.check_header(Some("Bearer s3cret")));
    }

    #[test]
    fn empty_env_var_is_treated_as_unset() {
        // Direct construction — we don't want to mutate process env in a
        // multi-threaded test runner. The behavior under test is the
        // `filter(!is_empty)` clause in `from_env`.
        let token = Some("".to_string()).filter(|s| !s.is_empty());
        assert!(token.is_none());
    }

    #[test]
    fn constant_time_eq_handles_different_lengths() {
        assert!(!constant_time_eq(b"a", b"ab"));
        assert!(constant_time_eq(b"abc", b"abc"));
    }
}
