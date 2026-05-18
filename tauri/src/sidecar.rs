//! Engine sidecar plumbing.
//!
//! Tauri owns the UI process. The Rust audio engine is a separate
//! binary (`hypehouse-engine`) — we spawn it as a child process on
//! app launch and tear it down on app exit.
//!
//! Why a separate process (not a linked-in module):
//!   * `cpal` opens a real-time audio thread. Co-tenanting that with
//!     the WebKit-backed Tauri runtime risks priority inversion on the
//!     audio callback when the renderer's GPU thread spikes.
//!   * Crash isolation: a panic in the audio thread shouldn't take
//!     down the UI mid-set.
//!   * Lets the engine binary keep being usable headless (CI / smoke
//!     tests / browser-only-mode dev) without dragging in the Tauri
//!     dependency tree.
//!
//! `ChildGuard` is a Drop-on-exit wrapper: when the Tauri app shuts
//! down, the guard runs `kill` + `wait` on the child so we don't leak
//! a zombie engine process.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use tracing::{info, warn};

/// Environment variable name the engine reads for its WS bearer token.
///
/// Mirrors what `engine/src/bridge/auth.rs` accepts. Documented so the
/// engine team can grep for the contract.
pub const ENGINE_TOKEN_ENV: &str = "HYPEHOUSE_BRIDGE_TOKEN";

/// Resolve the engine binary path relative to the desktop bundle.
///
/// Search order:
///   1. `HYPEHOUSE_ENGINE_BIN` env var (used by integration tests + power-users
///      who want to swap in a custom engine).
///   2. `../engine/target/release/hypehouse-engine[.exe]` — the workspace
///      layout when `cargo tauri dev` is invoked from `tauri/`.
///   3. Bundled sibling next to the desktop binary itself — what
///      `cargo tauri build` produces for distributables.
pub fn resolve_engine_path() -> PathBuf {
    if let Ok(p) = std::env::var("HYPEHOUSE_ENGINE_BIN") {
        return PathBuf::from(p);
    }

    let ext = if cfg!(windows) { ".exe" } else { "" };
    let dev_path = PathBuf::from(format!("../engine/target/release/hypehouse-engine{ext}"));
    if dev_path.exists() {
        return dev_path;
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join(format!("hypehouse-engine{ext}"));
            if sibling.exists() {
                return sibling;
            }
        }
    }

    // Fallback to the dev path even if it doesn't exist — the spawn
    // error message will be the right thing to surface.
    dev_path
}

/// Owns a spawned child process; kills + reaps it on Drop.
///
/// Cloneable handle: multiple Tauri command threads can share it via
/// `tauri::State`. The inner `Mutex<Option<Child>>` is taken once,
/// either by `kill_now` or by the Drop impl — whichever fires first.
#[derive(Clone)]
pub struct ChildGuard {
    inner: Arc<Mutex<Option<Child>>>,
    bin: PathBuf,
}

impl ChildGuard {
    /// Spawn `engine_bin` with `token` injected into its environment.
    ///
    /// stdout/stderr are captured + forwarded to the `tracing` logger
    /// on background threads. We keep the threads detached on purpose
    /// — they exit naturally when the child closes its pipes during
    /// kill+wait.
    pub fn spawn(engine_bin: PathBuf, token: &str) -> Result<Self> {
        info!(bin = %engine_bin.display(), "spawning engine sidecar");

        let mut cmd = Command::new(&engine_bin);
        cmd.env(ENGINE_TOKEN_ENV, token)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn engine at {}", engine_bin.display()))?;

        if let Some(stdout) = child.stdout.take() {
            spawn_log_forwarder("engine.stdout", stdout, false);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_forwarder("engine.stderr", stderr, true);
        }

        info!(pid = child.id(), "engine sidecar up");

        Ok(Self {
            inner: Arc::new(Mutex::new(Some(child))),
            bin: engine_bin,
        })
    }

    /// Spawn with an explicitly-constructed `Command`. Used by tests
    /// that want to point at a sleep-forever shim.
    #[doc(hidden)]
    pub fn spawn_with_command(mut cmd: Command, bin: PathBuf) -> Result<Self> {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn child at {}", bin.display()))?;
        if let Some(stdout) = child.stdout.take() {
            spawn_log_forwarder("child.stdout", stdout, false);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_forwarder("child.stderr", stderr, true);
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(child))),
            bin,
        })
    }

    /// Path of the binary backing this guard.
    pub fn bin(&self) -> &PathBuf {
        &self.bin
    }

    /// PID of the underlying child, or None if already reaped.
    pub fn pid(&self) -> Option<u32> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(Child::id))
    }

    /// Synchronously kill + reap the child. Idempotent: safe to call
    /// multiple times and from the Drop path.
    pub fn kill_now(&self) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(mut child) = guard.take() {
            let pid = child.id();
            if let Err(e) = child.kill() {
                // ESRCH (no such process) is fine — child already exited.
                warn!(pid, error = %e, "child kill failed (likely already exited)");
            }
            // wait reaps the zombie. Discard the ExitStatus; we don't act on it.
            let _ = child.wait();
            info!(pid, "engine sidecar killed + reaped");
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Only the last clone holds the unique Mutex contents; earlier
        // clones still see `Some(...)` but the Arc keeps them alive.
        // `kill_now` is a no-op once the child is taken.
        //
        // Note: Drop fires per-clone. We rely on Arc strong-count to
        // detect "last drop" — only then is there nothing left to share.
        if Arc::strong_count(&self.inner) == 1 {
            self.kill_now();
        }
    }
}

/// Reader thread that forwards each line from `pipe` to `tracing`.
///
/// `is_err` flag bumps the log level so engine panics aren't lost in
/// the noise of routine info-level lifecycle messages.
fn spawn_log_forwarder<R>(label: &'static str, pipe: R, is_err: bool)
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(pipe);
        for line in reader.lines().map_while(Result::ok) {
            if is_err {
                warn!(target: "sidecar", source = label, "{}", line);
            } else {
                info!(target: "sidecar", source = label, "{}", line);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_engine_path_respects_env_var() {
        // SAFETY: tests run single-threaded for env mutation purposes via
        // a dedicated key; even if cargo schedules parallel tests, each
        // test's value is read inside the same call so a race only
        // changes which path is picked, never causes UB.
        std::env::set_var("HYPEHOUSE_ENGINE_BIN", "/tmp/custom-engine-binary");
        let resolved = resolve_engine_path();
        std::env::remove_var("HYPEHOUSE_ENGINE_BIN");
        assert_eq!(resolved, PathBuf::from("/tmp/custom-engine-binary"));
    }

    #[test]
    fn child_guard_kill_is_idempotent() {
        // Use `true` (POSIX) / `cmd /C exit 0` (Windows) — both exit
        // instantly. We're proving kill_now doesn't panic when the
        // child has already exited or already been reaped.
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("cmd", vec!["/C", "exit", "0"])
        } else {
            ("true", vec![])
        };
        let mut cmd = Command::new(program);
        for a in &args {
            cmd.arg(a);
        }
        let guard =
            ChildGuard::spawn_with_command(cmd, PathBuf::from(program)).expect("spawn shim binary");
        // Give the child a moment to exit naturally.
        std::thread::sleep(std::time::Duration::from_millis(50));
        guard.kill_now();
        guard.kill_now(); // second call must not panic
        assert!(guard.pid().is_none(), "child should be reaped");
    }

    #[test]
    fn engine_token_env_constant_is_stable() {
        // Contract test: the engine bridge greps for this exact name.
        // Changing it requires a coordinated engine PR.
        assert_eq!(ENGINE_TOKEN_ENV, "HYPEHOUSE_BRIDGE_TOKEN");
    }

    #[test]
    fn resolve_engine_path_default_uses_dev_layout() {
        // Without the env var, the function returns the dev-layout
        // path (existence not required for the assertion — we only
        // care about the shape).
        std::env::remove_var("HYPEHOUSE_ENGINE_BIN");
        let resolved = resolve_engine_path();
        let s = resolved.to_string_lossy();
        assert!(
            s.contains("hypehouse-engine"),
            "expected resolved path to mention the engine binary name, got {s}"
        );
    }
}
