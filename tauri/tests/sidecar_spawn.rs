//! Integration test for the sidecar lifecycle.
//!
//! Strategy: spawn a "sleep forever" shim instead of the real engine
//! binary (which would require a release build of `engine/` + working
//! audio device — not portable in CI test runs). We assert that:
//!
//!   * `ChildGuard::spawn_with_command` returns Ok and the process is alive.
//!   * `kill_now` reaps the process so subsequent `pid()` returns None.
//!   * Drop on the last clone tears down the child even if `kill_now`
//!     was never explicitly called.
//!
//! Cross-platform: we use `sleep 30` on Unix and `cmd /C ping -n 31 127.0.0.1`
//! on Windows (timeout / sleep are unreliable in CI containers).
//!
//! Code under test lives in `src/sidecar.rs` and is reachable through
//! the library target declared in Cargo.toml.

use hypehouse_desktop::sidecar::{self, ChildGuard};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

fn sleep_command() -> (Command, PathBuf) {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "ping", "-n", "31", "127.0.0.1"]);
        (c, PathBuf::from("cmd.exe"))
    } else {
        let mut c = Command::new("sleep");
        c.arg("30");
        (c, PathBuf::from("/bin/sleep"))
    }
}

#[test]
fn sidecar_spawns_and_is_killable() {
    let (cmd, bin) = sleep_command();
    let guard = ChildGuard::spawn_with_command(cmd, bin).expect("spawn shim");

    // Process should be alive immediately after spawn.
    let pid = guard.pid().expect("pid available right after spawn");
    assert!(pid > 0, "expected a real PID, got {pid}");

    // Give the OS scheduler a moment so we know it's not a phantom.
    thread::sleep(Duration::from_millis(50));

    guard.kill_now();
    assert!(
        guard.pid().is_none(),
        "pid should be None after kill_now reaps the child"
    );
}

#[test]
fn sidecar_drop_kills_the_child() {
    let (cmd, bin) = sleep_command();
    let guard = ChildGuard::spawn_with_command(cmd, bin).expect("spawn shim");
    let pid = guard.pid().expect("pid present");

    // Drop the last clone — Drop impl should fire kill_now internally.
    drop(guard);

    // On Unix we can verify the PID is gone by sending signal 0.
    // On Windows we trust the kill+wait path (it's the same code).
    #[cfg(unix)]
    {
        // sleep a beat to let the OS reap.
        thread::sleep(Duration::from_millis(100));
        // SAFETY: kill(pid, 0) is a process-existence probe (no signal
        // delivered). No FFI safety concerns — the libc binding takes
        // two C ints and returns one.
        let alive = unsafe { libc_kill(pid as i32, 0) } == 0;
        assert!(!alive, "process {pid} still alive after ChildGuard drop");
    }

    // Suppress unused warning on Windows where the assertion above is
    // compiled out.
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

#[test]
fn sidecar_resolve_path_default_shape() {
    std::env::remove_var("HYPEHOUSE_ENGINE_BIN");
    let p = sidecar::resolve_engine_path();
    let s = p.to_string_lossy();
    assert!(
        s.contains("hypehouse-engine"),
        "default path should reference the engine binary, got {s}"
    );
}

#[test]
fn sidecar_resolve_path_honours_env() {
    std::env::set_var("HYPEHOUSE_ENGINE_BIN", "/var/empty/some-shim");
    let p = sidecar::resolve_engine_path();
    std::env::remove_var("HYPEHOUSE_ENGINE_BIN");
    assert_eq!(p, PathBuf::from("/var/empty/some-shim"));
}

// Minimal libc::kill binding (we don't want to pull in the `libc`
// crate as a dependency just for the existence probe).
#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
