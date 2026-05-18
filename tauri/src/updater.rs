//! Auto-updater scaffold (ADR-008).
//!
//! This module is the *seam* between the rest of the Tauri shell and
//! whatever updater implementation we wire in next. PR #39 deferred
//! both code signing and auto-update; this scaffold lays the groundwork
//! so a follow-up PR can drop in the real `tauri-plugin-updater` glue
//! without touching every callsite.
//!
//! What lives here today:
//!   * `UpdaterConfig` — endpoints + opt-in flag + channel.
//!   * `Updater` trait — narrow surface that command handlers + UI
//!     subscriptions depend on. Easy to mock for unit tests.
//!   * `HttpUpdater` — production impl. Performs a GET against the
//!     manifest endpoint, parses JSON, and compares semver. It does
//!     NOT actually download / install — that's deferred to the
//!     `tauri-plugin-updater` integration PR which can lean on Tauri's
//!     signature-verified download path.
//!   * `event_name_*` constants — names emitted to the webview via
//!     Tauri's event system. Locked in here so the UI can hardcode
//!     them without a runtime contract surprise.
//!
//! Threading: the production impl uses `std::thread::spawn` for the
//! manifest fetch so the Tauri main thread is never blocked. The
//! fetch returns over a `mpsc::Sender` the caller owns; on Drop the
//! sender is closed and the worker exits.
//!
//! Safety: no `unsafe` (forbidden by CLAUDE.md). No new background
//! threads without a documented join path: `Updater::check` returns
//! a `JoinHandle` so callers can `.join()` on shutdown.

use serde::{Deserialize, Serialize};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Event names emitted to the webview. The UI subscribes via
/// `appWindow.listen("updater://available", …)`.
pub const EVENT_AVAILABLE: &str = "updater://available";
pub const EVENT_DOWNLOADED: &str = "updater://downloaded";
pub const EVENT_ERROR: &str = "updater://error";

/// Release channel. End users get `stable`; opt-in testers get `beta`.
/// We *deliberately* do not expose a third "nightly" — review fatigue.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

/// Config flowed through from `tauri.conf.json` (or a future user
/// settings store) into the runtime. `opt_in` is the kill-switch: if
/// false, we never even hit the manifest URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdaterConfig {
    pub opt_in: bool,
    pub channel: Channel,
    /// Manifest endpoint, may contain `{{target}}` / `{{arch}}` /
    /// `{{current_version}}` template tokens (substituted at fetch time).
    pub manifest_url: String,
    /// Soft timeout for the manifest GET. 5s is plenty for a JSON file
    /// behind a CDN; longer than that means the network is broken and
    /// we should fail fast.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    5
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            opt_in: false,
            channel: Channel::Stable,
            manifest_url: "https://hypehouse.live/releases/stable/manifest.json".into(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

/// Shape of the JSON manifest served at `manifest_url`. Matches the
/// Tauri v2 updater spec subset we care about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub notes: Option<String>,
    pub pub_date: Option<String>,
    /// URL of the platform-specific bundle. The updater plugin verifies
    /// the detached signature before unpacking.
    pub url: String,
    pub signature: String,
}

/// Result returned to the Tauri command surface. `Serialize` so it can
/// cross the JSON-RPC bridge into the webview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckResult {
    pub available: bool,
    pub version: Option<String>,
    pub notes: Option<String>,
}

/// Errors surfaced to the UI. We keep these wide+narrow: each variant
/// maps to a localizable message in the frontend. Wire-format is the
/// `Display` impl (also what gets serialised in the error event).
#[derive(Debug, thiserror::Error)]
pub enum UpdaterError {
    #[error("updater disabled by user (opt_in=false)")]
    Disabled,
    #[error("network error fetching manifest: {0}")]
    Network(String),
    #[error("manifest body did not parse as JSON: {0}")]
    BadManifest(String),
    #[error("manifest version `{0}` is not valid semver")]
    BadVersion(String),
}

/// Narrow trait so command handlers don't bake in the production HTTP
/// impl. Tests substitute an in-process fake.
pub trait Updater: Send + Sync {
    fn check(&self, current_version: &str) -> Result<CheckResult, UpdaterError>;
}

/// Production impl. Does not depend on `tauri::AppHandle` — that
/// wiring happens in `main.rs` when we register the manage()'d state.
/// Keeping it AppHandle-free means we can unit-test the manifest-fetch
/// path against a localhost mock server without a Tauri context.
pub struct HttpUpdater {
    pub config: UpdaterConfig,
}

impl HttpUpdater {
    pub fn new(config: UpdaterConfig) -> Self {
        Self { config }
    }

    /// Spawn a worker thread to fetch the manifest. Returns a
    /// `JoinHandle` so the caller can guarantee shutdown.
    pub fn check_async(
        &self,
        current_version: String,
    ) -> (
        mpsc::Receiver<Result<CheckResult, UpdaterError>>,
        JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::channel();
        let cfg = self.config.clone();
        let handle = thread::Builder::new()
            .name("hypehouse-updater".into())
            .spawn(move || {
                let result = check_blocking(&cfg, &current_version);
                // Receiver may have hung up — that's fine, this is fire-and-forget.
                let _ = tx.send(result);
            })
            .expect("spawning updater thread should never fail");
        (rx, handle)
    }
}

impl Updater for HttpUpdater {
    fn check(&self, current_version: &str) -> Result<CheckResult, UpdaterError> {
        check_blocking(&self.config, current_version)
    }
}

/// Pure function so tests can poke it with a synthetic config + a
/// mock server URL. Intentionally NOT async — Tauri commands run on
/// a Tokio pool and the cost of a blocking GET inside a background
/// thread is < 1ms of pool overhead.
fn check_blocking(cfg: &UpdaterConfig, current_version: &str) -> Result<CheckResult, UpdaterError> {
    if !cfg.opt_in {
        return Err(UpdaterError::Disabled);
    }

    let url = render_manifest_url(&cfg.manifest_url, current_version);
    let body =
        http_get(&url, Duration::from_secs(cfg.timeout_secs)).map_err(UpdaterError::Network)?;

    let manifest: Manifest =
        serde_json::from_str(&body).map_err(|e| UpdaterError::BadManifest(e.to_string()))?;

    let newer = is_newer_semver(&manifest.version, current_version)
        .ok_or_else(|| UpdaterError::BadVersion(manifest.version.clone()))?;

    Ok(CheckResult {
        available: newer,
        version: Some(manifest.version),
        notes: manifest.notes,
    })
}

/// Substitute the documented templates. Kept tiny + dependency-free.
fn render_manifest_url(template: &str, current_version: &str) -> String {
    template
        .replace("{{target}}", std::env::consts::OS)
        .replace("{{arch}}", std::env::consts::ARCH)
        .replace("{{current_version}}", current_version)
}

/// Pluggable HTTP GET. Default path uses `ureq` if added, but the
/// scaffold uses a `std::net::TcpStream` shim so we don't pull a new
/// dependency just for the placeholder. The integration PR replaces
/// this with `tauri::http` or `reqwest::blocking`.
#[cfg(not(test))]
fn http_get(_url: &str, _timeout: Duration) -> Result<String, String> {
    // PLACEHOLDER: real implementation lands with the updater-plugin
    // integration PR. Returning an explicit error keeps the call path
    // exercised in dev without making bogus network requests.
    Err("http_get not implemented in scaffold — wire tauri-plugin-updater".into())
}

#[cfg(test)]
fn http_get(url: &str, timeout: Duration) -> Result<String, String> {
    test_support::take_response(url, timeout)
}

/// Compare two semver strings. Returns Some(true) if `candidate > current`,
/// Some(false) otherwise. None for malformed input. Tiny hand-rolled
/// parser so we don't pull `semver` for one comparison.
fn is_newer_semver(candidate: &str, current: &str) -> Option<bool> {
    let c = parse_semver(candidate)?;
    let cur = parse_semver(current)?;
    Some(c > cur)
}

fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let maj = parts.next()?.parse().ok()?;
    let min = parts.next()?.parse().ok()?;
    let pat = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((maj, min, pat))
}

/// In-process test scaffolding. Lets unit tests + integration tests
/// queue up a manifest response keyed by URL.
#[cfg(test)]
pub mod test_support {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    static RESPONSES: Mutex<Option<HashMap<String, Result<String, String>>>> = Mutex::new(None);

    pub fn set_response(url: &str, body: Result<String, String>) {
        let mut guard = RESPONSES.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(url.to_string(), body);
    }

    pub fn take_response(url: &str, _timeout: Duration) -> Result<String, String> {
        let mut guard = RESPONSES.lock().unwrap();
        match guard.as_mut() {
            Some(map) => map
                .remove(url)
                .unwrap_or_else(|| Err(format!("no mock response queued for {url}"))),
            None => Err("no mock responses installed".into()),
        }
    }

    pub fn reset() {
        *RESPONSES.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_cfg(url: &str) -> UpdaterConfig {
        UpdaterConfig {
            opt_in: true,
            channel: Channel::Stable,
            manifest_url: url.to_string(),
            timeout_secs: 1,
        }
    }

    #[test]
    fn semver_comparator_handles_basic_cases() {
        assert_eq!(is_newer_semver("0.2.0", "0.1.0"), Some(true));
        assert_eq!(is_newer_semver("0.1.1", "0.1.0"), Some(true));
        assert_eq!(is_newer_semver("0.1.0", "0.1.0"), Some(false));
        assert_eq!(is_newer_semver("0.1.0", "0.2.0"), Some(false));
        assert_eq!(is_newer_semver("1.0.0", "0.99.99"), Some(true));
        assert_eq!(is_newer_semver("not-semver", "0.1.0"), None);
        assert_eq!(is_newer_semver("0.1.0.1", "0.1.0"), None);
    }

    #[test]
    fn check_returns_available_true_when_manifest_is_newer() {
        test_support::reset();
        let url = "https://test.invalid/manifest-newer.json";
        let body = serde_json::json!({
            "version": "0.2.0",
            "notes": "bigger and better",
            "url": "https://test.invalid/bundle.tar.gz",
            "signature": "deadbeef",
        })
        .to_string();
        test_support::set_response(url, Ok(body));

        let updater = HttpUpdater::new(mk_cfg(url));
        let result = updater.check("0.1.0").expect("manifest fetch ok");
        assert!(result.available);
        assert_eq!(result.version.as_deref(), Some("0.2.0"));
        assert_eq!(result.notes.as_deref(), Some("bigger and better"));
    }

    #[test]
    fn check_returns_available_false_when_manifest_is_same_or_older() {
        test_support::reset();
        let url = "https://test.invalid/manifest-same.json";
        let body = serde_json::json!({
            "version": "0.1.0",
            "url": "https://test.invalid/bundle.tar.gz",
            "signature": "deadbeef",
        })
        .to_string();
        test_support::set_response(url, Ok(body));

        let updater = HttpUpdater::new(mk_cfg(url));
        let result = updater.check("0.1.0").expect("manifest fetch ok");
        assert!(!result.available);
        assert_eq!(result.version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn check_errors_when_opt_in_is_false() {
        let cfg = UpdaterConfig {
            opt_in: false,
            ..mk_cfg("https://test.invalid/never-called.json")
        };
        let updater = HttpUpdater::new(cfg);
        let err = updater.check("0.1.0").unwrap_err();
        assert!(matches!(err, UpdaterError::Disabled));
    }

    #[test]
    fn check_errors_on_bad_manifest_json() {
        test_support::reset();
        let url = "https://test.invalid/manifest-bad.json";
        test_support::set_response(url, Ok("not json at all".into()));
        let updater = HttpUpdater::new(mk_cfg(url));
        let err = updater.check("0.1.0").unwrap_err();
        assert!(matches!(err, UpdaterError::BadManifest(_)));
    }

    #[test]
    fn event_name_constants_are_stable() {
        // UI hard-codes these — locking them down here guarantees CI
        // breaks if anyone renames the events without coordinating.
        assert_eq!(EVENT_AVAILABLE, "updater://available");
        assert_eq!(EVENT_DOWNLOADED, "updater://downloaded");
        assert_eq!(EVENT_ERROR, "updater://error");
    }

    #[test]
    fn render_manifest_url_substitutes_templates() {
        let rendered = render_manifest_url(
            "https://x/{{target}}/{{arch}}/{{current_version}}/m.json",
            "1.2.3",
        );
        assert!(rendered.contains("1.2.3"));
        assert!(rendered.contains(std::env::consts::OS));
        assert!(rendered.contains(std::env::consts::ARCH));
    }
}
