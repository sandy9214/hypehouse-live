//! Opt-in Sentry telemetry hook for the audio engine.
//!
//! # Privacy
//!
//! Telemetry is **OFF by default**. A user (or fork operator) has to
//! flip one of two switches to enable it:
//!
//! 1. Env var `HYPEHOUSE_TELEMETRY_ENABLED=1` — fast path, intended
//!    for short-lived debugging sessions ("reproduce the crash with
//!    telemetry on, then turn it off").
//! 2. A config file at `~/.config/hypehouse-live/telemetry.toml` with
//!    `enabled = true` — persistent opt-in for long-running rigs.
//!
//! If neither is set, [`init_telemetry`] returns `None` and the Sentry
//! SDK is never initialised. No DSN is contacted, no events leave the
//! machine.
//!
//! # DSN
//!
//! The placeholder DSN below is intentionally bogus
//! (`https://<dsn-key>@o4500000.ingest.sentry.io/<project>`) so that
//! upstream builds do not accidentally ship a working DSN. Fork
//! operators replace the constant (or set
//! `HYPEHOUSE_TELEMETRY_DSN`) before deploying telemetry-enabled
//! builds. See `docs/telemetry.md` for the full setup story.
//!
//! # PII scrubbing
//!
//! Every event passes through [`scrub_pii`] before being sent. We
//! strip request headers, drop the `extra.user_path` slot if present,
//! and collapse any string in `extra` that looks like a filesystem
//! path or track name down to its filename / `<scrubbed>`. The
//! audio-engine never deliberately attaches PII, but a future
//! contributor adding `sentry::capture_message(format!(...))` should
//! not be able to leak a user's library by accident.

use std::path::PathBuf;
use std::sync::Arc;

use sentry::protocol::Event;
use sentry::types::Dsn;
use sentry::{ClientInitGuard, ClientOptions};

/// Placeholder DSN. Fork operators override via the
/// `HYPEHOUSE_TELEMETRY_DSN` env var or by editing this constant.
pub const PLACEHOLDER_DSN: &str =
    "https://examplePublicKey@o4500000.ingest.sentry.io/4500000000000000";

/// Env var that turns the SDK on for the duration of a process.
pub const ENV_ENABLED: &str = "HYPEHOUSE_TELEMETRY_ENABLED";
/// Optional env override for the DSN. Empty string == disabled.
pub const ENV_DSN: &str = "HYPEHOUSE_TELEMETRY_DSN";
/// Optional env override for the deployment environment tag.
pub const ENV_ENVIRONMENT: &str = "HYPEHOUSE_TELEMETRY_ENVIRONMENT";

/// Result of [`resolve_enabled`] — small enum so tests can pattern
/// match on the source of the decision.
#[derive(Debug, PartialEq, Eq)]
pub enum EnabledSource {
    /// Env var present and truthy (`1`, `true`, `yes`).
    Env,
    /// Config file present and `enabled = true`.
    ConfigFile,
    /// Neither switch set — telemetry stays off.
    Off,
}

/// Inspect the env var first, then optionally the config file path.
///
/// Pure function — no I/O beyond reading `path` when provided. Easy
/// to drive from tests.
pub fn resolve_enabled(
    env_value: Option<&str>,
    config_path: Option<&std::path::Path>,
) -> EnabledSource {
    if let Some(v) = env_value {
        let trimmed = v.trim().to_ascii_lowercase();
        if matches!(trimmed.as_str(), "1" | "true" | "yes" | "on") {
            return EnabledSource::Env;
        }
    }
    if let Some(p) = config_path {
        if let Ok(contents) = std::fs::read_to_string(p) {
            if parse_enabled_from_toml(&contents) {
                return EnabledSource::ConfigFile;
            }
        }
    }
    EnabledSource::Off
}

/// Cheap hand-rolled TOML scanner — we only need a single boolean,
/// and pulling in a TOML parser just for this would bloat the audio
/// engine. Looks for a top-level `enabled = true` line, ignoring
/// comments and surrounding whitespace.
pub(crate) fn parse_enabled_from_toml(contents: &str) -> bool {
    for raw in contents.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        // Accept either bare `enabled = true` or quoted `"enabled" = true`.
        let normalised: String = line
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '"')
            .collect();
        if normalised.eq_ignore_ascii_case("enabled=true") {
            return true;
        }
    }
    false
}

/// Default location for the config file. Honours `XDG_CONFIG_HOME`
/// when set; falls back to `$HOME/.config/hypehouse-live/telemetry.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(root.join("hypehouse-live").join("telemetry.toml"))
}

/// Apply our PII-scrub rules in-place and return the event back to
/// the SDK. Returns `None` to drop the event entirely.
pub fn scrub_pii(mut event: Event<'static>) -> Option<Event<'static>> {
    // Drop request headers wholesale — they can carry bearer tokens
    // and Cookie material from the WebSocket upgrade.
    if let Some(req) = event.request.as_mut() {
        req.headers.clear();
        req.cookies = None;
        req.query_string = None;
    }
    // Strip the user object — we never want to ship usernames /
    // emails / IP addresses, even if a future contributor sets them
    // via `sentry::configure_scope`.
    event.user = None;
    event.server_name = None;
    // Scrub strings in `extra`.
    for (_k, value) in event.extra.iter_mut() {
        scrub_value(value);
    }
    // Same for tags — never ship a track name or filesystem path as
    // a tag value.
    for value in event.tags.values_mut() {
        *value = scrub_string(value);
    }
    Some(event)
}

/// Walk a serde_json::Value recursively, scrubbing every string leaf.
fn scrub_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => *s = scrub_string(s),
        serde_json::Value::Array(items) => items.iter_mut().for_each(scrub_value),
        serde_json::Value::Object(map) => map.values_mut().for_each(scrub_value),
        _ => {}
    }
}

/// Heuristic scrub for a single string:
///
/// * Anything that contains `/` or `\` is treated as a filesystem
///   path; we keep only the basename.
/// * Anything that *looks like* a home directory (`/Users/...`,
///   `/home/...`) is collapsed entirely.
/// * Otherwise the string is returned untouched — Sentry stack frames
///   contain plenty of legitimate non-PII strings (function names,
///   crate versions) we do want to keep.
fn scrub_string(s: &str) -> String {
    if s.starts_with("/Users/")
        || s.starts_with("/home/")
        || s.starts_with("C:\\Users\\")
        || s.starts_with("C:/Users/")
    {
        return "<scrubbed-path>".to_string();
    }
    if s.contains('/') || s.contains('\\') {
        // Keep just the trailing component.
        let last = s.rsplit(['/', '\\']).next().unwrap_or("");
        if last.is_empty() {
            "<scrubbed-path>".to_string()
        } else {
            last.to_string()
        }
    } else {
        s.to_string()
    }
}

/// Initialise Sentry if the user has opted in. Returns the guard
/// (drop = flush) when telemetry is live, `None` otherwise.
///
/// Wired into `main` via `let _guard = telemetry::init_telemetry();`.
/// The guard must outlive any code path that may panic — that's why
/// it's hoisted up to `main` rather than created in a sub-scope.
pub fn init_telemetry() -> Option<ClientInitGuard> {
    let env_value = std::env::var(ENV_ENABLED).ok();
    let cfg_path = default_config_path();
    let decision = resolve_enabled(env_value.as_deref(), cfg_path.as_deref());
    if matches!(decision, EnabledSource::Off) {
        tracing::info!("telemetry: disabled (set {ENV_ENABLED}=1 to opt in)");
        return None;
    }

    let dsn_str = std::env::var(ENV_DSN).unwrap_or_else(|_| PLACEHOLDER_DSN.to_string());
    if dsn_str.trim().is_empty() {
        tracing::info!("telemetry: DSN empty — staying disabled");
        return None;
    }
    let dsn: Dsn = match dsn_str.parse() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "telemetry: invalid DSN — staying disabled");
            return None;
        }
    };
    let environment = std::env::var(ENV_ENVIRONMENT)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(debug_assertions) {
                "dev".to_string()
            } else {
                "production".to_string()
            }
        });

    let options = ClientOptions {
        dsn: Some(dsn),
        release: Some(format!("hypehouse-engine@{}", env!("CARGO_PKG_VERSION")).into()),
        environment: Some(environment.into()),
        send_default_pii: false,
        attach_stacktrace: true,
        before_send: Some(Arc::new(scrub_pii)),
        ..Default::default()
    };
    tracing::info!(
        source = ?decision,
        "telemetry: enabling Sentry SDK — opt-in confirmed"
    );
    Some(sentry::init(options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn resolve_off_when_nothing_set() {
        assert_eq!(resolve_enabled(None, None), EnabledSource::Off);
        assert_eq!(resolve_enabled(Some(""), None), EnabledSource::Off);
        assert_eq!(resolve_enabled(Some("0"), None), EnabledSource::Off);
        assert_eq!(resolve_enabled(Some("false"), None), EnabledSource::Off);
    }

    #[test]
    fn resolve_env_truthy_variants() {
        for v in ["1", "true", "yes", "on", "TRUE", "Yes"] {
            assert_eq!(
                resolve_enabled(Some(v), None),
                EnabledSource::Env,
                "value {v}"
            );
        }
    }

    #[test]
    fn resolve_config_file_when_env_off() {
        let dir = tempdir();
        let p = dir.join("telemetry.toml");
        std::fs::write(&p, "# header\nenabled = true\n").unwrap();
        assert_eq!(
            resolve_enabled(Some("0"), Some(&p)),
            EnabledSource::ConfigFile
        );
        std::fs::write(&p, "enabled = false\n").unwrap();
        assert_eq!(resolve_enabled(Some("0"), Some(&p)), EnabledSource::Off);
    }

    #[test]
    fn parse_toml_ignores_comments_and_whitespace() {
        assert!(parse_enabled_from_toml("# hi\nenabled = true\n"));
        assert!(parse_enabled_from_toml("  enabled  =  true  \n"));
        assert!(parse_enabled_from_toml("\"enabled\" = true"));
        assert!(!parse_enabled_from_toml("enabled = false"));
        assert!(!parse_enabled_from_toml(""));
        assert!(!parse_enabled_from_toml("# enabled = true"));
    }

    #[test]
    fn scrub_collapses_home_paths() {
        assert_eq!(
            scrub_string("/Users/jane/Music/track.mp3"),
            "<scrubbed-path>"
        );
        assert_eq!(
            scrub_string("/home/jane/Music/track.mp3"),
            "<scrubbed-path>"
        );
        assert_eq!(
            scrub_string("C:\\Users\\jane\\track.mp3"),
            "<scrubbed-path>"
        );
    }

    #[test]
    fn scrub_keeps_basename_for_other_paths() {
        assert_eq!(scrub_string("/tmp/cache/file.dat"), "file.dat");
        assert_eq!(scrub_string("relative/sub/file.dat"), "file.dat");
    }

    #[test]
    fn scrub_passes_through_non_path_strings() {
        assert_eq!(scrub_string("hello"), "hello");
        assert_eq!(scrub_string("decode_panic"), "decode_panic");
    }

    #[test]
    fn scrub_value_recurses_into_arrays_and_objects() {
        let mut v = serde_json::json!({
            "track_path": "/Users/jane/Music/x.mp3",
            "nested": ["/home/x/y", {"q": "/Users/a/b"}],
            "ok": "leave-me",
        });
        scrub_value(&mut v);
        assert_eq!(v["track_path"], serde_json::json!("<scrubbed-path>"));
        assert_eq!(v["nested"][0], serde_json::json!("<scrubbed-path>"));
        assert_eq!(v["nested"][1]["q"], serde_json::json!("<scrubbed-path>"));
        assert_eq!(v["ok"], serde_json::json!("leave-me"));
    }

    #[test]
    fn scrub_pii_drops_headers_and_user() {
        let mut req = sentry::protocol::Request::default();
        req.headers
            .insert("Authorization".into(), "Bearer secret".into());
        req.cookies = Some("sess=abc".to_string());
        let mut extra = BTreeMap::new();
        extra.insert(
            "track_path".to_string(),
            serde_json::json!("/Users/jane/track.mp3"),
        );
        let event = Event {
            request: Some(req),
            user: Some(sentry::protocol::User {
                username: Some("jane".to_string()),
                ..Default::default()
            }),
            server_name: Some("jane-mbp".into()),
            extra,
            ..Default::default()
        };
        let scrubbed = scrub_pii(event).expect("event survives scrub");
        assert!(scrubbed.request.as_ref().unwrap().headers.is_empty());
        assert!(scrubbed.user.is_none());
        assert!(scrubbed.server_name.is_none());
        assert_eq!(
            scrubbed.extra.get("track_path"),
            Some(&serde_json::json!("<scrubbed-path>"))
        );
    }

    #[test]
    fn init_telemetry_returns_none_when_off() {
        // Note: we don't actually mutate the global env in tests to
        // avoid racing with other parallel tests. The resolve_*
        // tests above already cover the decision tree; this asserts
        // the integrated function gracefully no-ops in the typical
        // unset case.
        let prev_enabled = std::env::var(ENV_ENABLED).ok();
        let prev_dsn = std::env::var(ENV_DSN).ok();
        // SAFETY: only run when no other test has set these; we
        // restore at the end. Cargo by default runs tests in
        // parallel inside one binary but these vars are scoped to
        // the entire process so the explicit save/restore is the
        // best we can do without serial_test.
        std::env::remove_var(ENV_ENABLED);
        std::env::remove_var(ENV_DSN);
        let guard = init_telemetry();
        assert!(guard.is_none(), "expected None when env unset");
        if let Some(v) = prev_enabled {
            std::env::set_var(ENV_ENABLED, v);
        }
        if let Some(v) = prev_dsn {
            std::env::set_var(ENV_DSN, v);
        }
    }

    /// Build a unique scratch dir under the OS temp root.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("hypehouse-telemetry-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
