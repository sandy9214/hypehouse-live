//! Disk persistence for the event-sourced engine log (ADR-003).
//!
//! ADR-003 specifies an append-only event log as the single source of
//! truth for live engine state. Until now the log lived only in memory:
//! a crash mid-set lost the entire night's mix history, which makes
//! post-mortem debugging ("why did deck B drop out at 23:47?")
//! impossible. This module persists every event to a per-session JSONL
//! file under the user's XDG_DATA_HOME so:
//!
//! 1. **Survivability**: a crash mid-set leaves a complete, human-
//!    readable timeline on disk.
//! 2. **Replay**: the same events can be re-folded into [`EngineState`]
//!    on a dev machine to reproduce the bug exactly. See
//!    [`replay::replay_state`].
//! 3. **Auditability**: JSONL is grep-friendly. `jq` works. No custom
//!    tooling required to ask "did Crossfader ever go to 1.0?".
//!
//! Layout
//! ------
//! ```text
//! $HYPEHOUSE_EVENT_LOG_DIR or
//! $XDG_DATA_HOME/hypehouse-live/sessions or
//! ~/.local/share/hypehouse-live/sessions
//!     20260518T013312Z-a4f2/
//!         events.jsonl
//! ```
//!
//! Threading
//! ---------
//! [`EventLog::append`] is called from the **control thread only** —
//! never from the audio thread (ADR-004 forbids syscalls there). The
//! append cost is bounded by a [`BufWriter`] — typical Linux page
//! cache hit is sub-microsecond, syscalls only on flush. The flush
//! cadence is `FLUSH_EVERY_N` events; the [`Drop`] impl flushes the
//! tail. No background thread; no shared state.
//!
//! Env config
//! ----------
//! * `HYPEHOUSE_EVENT_LOG_DIR` — override storage root (default
//!   `$XDG_DATA_HOME/hypehouse-live/sessions` or
//!   `~/.local/share/hypehouse-live/sessions`).
//! * `HYPEHOUSE_EVENT_LOG_DISABLED=1` — skip writes entirely. Used by
//!   the test suite + ephemeral runs.

pub mod replay;

use crate::state::Event;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Flush the buffer to disk every N events. Trades tail loss on a
/// hard crash (last <N events) against syscall amortisation. 100 ≈
/// 10s of brisk live activity; small enough to bound loss, large
/// enough to keep syscall pressure off the control thread.
pub const FLUSH_EVERY_N: u64 = 100;

/// Environment variable enabling override of the storage root.
pub const ENV_LOG_DIR: &str = "HYPEHOUSE_EVENT_LOG_DIR";
/// Environment variable that disables writes entirely. Set to `1`.
pub const ENV_LOG_DISABLED: &str = "HYPEHOUSE_EVENT_LOG_DISABLED";

/// Persistent append-only JSONL log of [`Event`]s for one session.
///
/// Construct with [`EventLog::new`]. Call [`EventLog::append`] from
/// the control thread after each successful reducer apply. Drop the
/// log on shutdown — the [`Drop`] impl flushes the writer.
///
/// When the disabled-mode env var is set, [`EventLog::new`] still
/// returns successfully but holds no file handle; [`EventLog::append`]
/// becomes a no-op. This keeps the call sites in `main.rs` simple
/// (no `Option<EventLog>` plumbing).
pub struct EventLog {
    /// `None` when disabled (no file open) — calls become no-ops.
    writer: Option<BufWriter<File>>,
    /// Resolved path to the events file, or `None` when disabled.
    path: Option<PathBuf>,
    /// Count of events appended since the last flush.
    since_flush: u64,
    /// Session identifier used to construct the directory.
    session_id: String,
}

impl EventLog {
    /// Open (or create) the event log for `session_id`.
    ///
    /// Creates the per-session directory under the resolved storage
    /// root if it does not yet exist. Opens `events.jsonl` with
    /// `O_APPEND | O_CREAT` so multiple processes appending to the
    /// same file would interleave safely (we don't do that, but the
    /// kernel-level atomicity is a defensive default).
    ///
    /// Honours [`ENV_LOG_DISABLED`] — when set to `1`, returns an
    /// `EventLog` whose `append` is a no-op and which never touches
    /// the filesystem.
    pub fn new(session_id: &str) -> Result<Self> {
        if std::env::var(ENV_LOG_DISABLED).as_deref() == Ok("1") {
            return Ok(Self {
                writer: None,
                path: None,
                since_flush: 0,
                session_id: session_id.to_string(),
            });
        }

        let root = resolve_log_root()?;
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir)
            .with_context(|| format!("creating event log dir {}", session_dir.display()))?;

        let path = session_dir.join("events.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening event log file {}", path.display()))?;

        Ok(Self {
            writer: Some(BufWriter::new(file)),
            path: Some(path),
            since_flush: 0,
            session_id: session_id.to_string(),
        })
    }

    /// Append a single event as one JSON line. Flushes every
    /// [`FLUSH_EVERY_N`] events.
    ///
    /// Errors only when serde_json serialization itself fails (which
    /// can't happen for our `Event` schema) or the file write returns
    /// an error. Callers should log + continue rather than abort the
    /// engine on append failure — losing the log is degraded mode,
    /// not unsafe (audit trail is best-effort, not safety-critical).
    pub fn append(&mut self, event: &Event) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(()); // disabled mode
        };
        let mut line =
            serde_json::to_vec(event).context("serializing Event to JSON for event log")?;
        line.push(b'\n');
        writer
            .write_all(&line)
            .context("writing event line to event log")?;
        self.since_flush += 1;
        if self.since_flush >= FLUSH_EVERY_N {
            writer.flush().context("flushing event log buffer")?;
            self.since_flush = 0;
        }
        Ok(())
    }

    /// Force a flush of the underlying [`BufWriter`]. No-op in
    /// disabled mode. Called by [`Drop`] and externally on graceful
    /// shutdown.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.flush().context("flushing event log buffer")?;
            self.since_flush = 0;
        }
        Ok(())
    }

    /// Resolved on-disk path. `None` in disabled mode.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Session ID this log was opened for.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// True when the log is in disabled mode (no file write).
    pub fn is_disabled(&self) -> bool {
        self.writer.is_none()
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            // Best-effort; we can't propagate errors out of Drop. The
            // BufWriter's own Drop also flushes, but we explicitly do
            // it here so any error surfaces in tracing rather than
            // being silently swallowed.
            if let Err(e) = writer.flush() {
                tracing::warn!(error = %e, "event log: flush on drop failed");
            }
        }
    }
}

/// Resolve the storage root directory in priority order:
/// 1. `HYPEHOUSE_EVENT_LOG_DIR` env var
/// 2. `$XDG_DATA_HOME/hypehouse-live/sessions`
/// 3. `$HOME/.local/share/hypehouse-live/sessions`
fn resolve_log_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var(ENV_LOG_DIR) {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return Ok(PathBuf::from(xdg).join("hypehouse-live").join("sessions"));
        }
    }
    let home = std::env::var("HOME").context("resolving $HOME for event log root")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("hypehouse-live")
        .join("sessions"))
}

/// Generate a fresh session id of the form `YYYYMMDDTHHMMSSZ-XXXX`
/// where `XXXX` is 4 lowercase hex chars derived from a non-crypto
/// random source (process-time entropy — collisions on the same
/// machine within the same second are extremely unlikely and a
/// collision would only mean appending to an existing session file,
/// not corruption).
pub fn new_session_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let nanos = now.subsec_nanos();
    let (y, mo, d, h, mi, s) = unix_secs_to_utc(secs as i64);
    let suffix = hex_suffix(nanos ^ std::process::id());
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z-{suffix}")
}

/// Convert unix seconds → `(year, month, day, hour, minute, second)`
/// in UTC. Plain civil calendar math — accurate for 1970..9999. Kept
/// inline rather than pulling chrono just for an ISO timestamp.
fn unix_secs_to_utc(t: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86400);
    let secs_of_day = t.rem_euclid(86400) as u32;
    let h = secs_of_day / 3600;
    let mi = (secs_of_day / 60) % 60;
    let s = secs_of_day % 60;

    // Howard Hinnant's date algorithm — public-domain civil calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let yy = y + i64::from(m <= 2);
    (yy as i32, m, d, h, mi, s)
}

/// Render `seed` as 4 lowercase hex chars (top 16 bits).
fn hex_suffix(seed: u32) -> String {
    let v = (seed >> 16) & 0xFFFF;
    format!("{v:04x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeckId, EngineState, EventKind, EventSource};
    use std::io::{BufRead, BufReader};

    /// Snapshot the current `HYPEHOUSE_EVENT_LOG_*` env vars and
    /// restore them on drop. Tests share the process env so each test
    /// must reset it cleanly.
    struct EnvGuard {
        log_dir: Option<String>,
        disabled: Option<String>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                log_dir: std::env::var(ENV_LOG_DIR).ok(),
                disabled: std::env::var(ENV_LOG_DISABLED).ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation across tests is serialized by the
            // test_lock mutex — see the lock() helper below.
            match &self.log_dir {
                Some(v) => std::env::set_var(ENV_LOG_DIR, v),
                None => std::env::remove_var(ENV_LOG_DIR),
            }
            match &self.disabled {
                Some(v) => std::env::set_var(ENV_LOG_DISABLED, v),
                None => std::env::remove_var(ENV_LOG_DISABLED),
            }
        }
    }

    /// Per-test mutex so env-mutating tests don't race. cargo test
    /// runs in parallel by default.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Build a deterministic per-test scratch directory under
    /// `std::env::temp_dir()`. Using temp_dir avoids a `tempfile`
    /// dependency. Returns the path; cleaned up by the test
    /// explicitly so we can inspect contents on failure.
    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("hh-evlog-{tag}-{pid}-{nanos}"));
        fs::create_dir_all(&dir).expect("scratch dir create");
        dir
    }

    fn ev(id: u64, kind: EventKind) -> Event {
        Event {
            id,
            ts_micros: id as i64 * 1000,
            source: EventSource::Ui,
            kind,
        }
    }

    #[test]
    fn round_trip_100_events_matches_in_memory_apply() {
        let _g = test_lock();
        let _env = EnvGuard::new();
        let dir = scratch_dir("rt100");
        std::env::set_var(ENV_LOG_DIR, &dir);
        std::env::remove_var(ENV_LOG_DISABLED);

        let sid = "test-rt100";
        let events: Vec<Event> = (0..100u64)
            .map(|i| {
                let kind = match i % 4 {
                    0 => EventKind::DeckPlay { deck: DeckId::A },
                    1 => EventKind::DeckPause { deck: DeckId::A },
                    2 => EventKind::Crossfader {
                        value: (i as f32) / 100.0,
                    },
                    _ => EventKind::PhaseNudge {
                        deck: DeckId::B,
                        delta_ms: (i as i32) % 5,
                    },
                };
                ev(i, kind)
            })
            .collect();

        // In-memory fold for the reference state.
        let mut expected = EngineState::default();
        for e in &events {
            expected = expected.apply(e);
        }

        // Write through EventLog.
        {
            let mut log = EventLog::new(sid).expect("open log");
            for e in &events {
                log.append(e).expect("append");
            }
        } // Drop flushes.

        // Replay from disk and verify state matches.
        let path = dir.join(sid).join("events.jsonl");
        let replayed: Vec<Event> = replay::EventReplay::open(&path)
            .expect("open replay")
            .collect()
            .expect("collect replay");
        assert_eq!(replayed.len(), events.len());
        let folded = replay::replay_state(&replayed);
        // Direct field comparison — Eq isn't derived on EngineState.
        assert_eq!(folded.deck_a.playing, expected.deck_a.playing);
        assert_eq!(
            folded.deck_b.phase_offset_ms,
            expected.deck_b.phase_offset_ms
        );
        assert!((folded.crossfader - expected.crossfader).abs() < 1e-6);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonl_format_one_line_per_event_with_expected_fields() {
        let _g = test_lock();
        let _env = EnvGuard::new();
        let dir = scratch_dir("jsonl");
        std::env::set_var(ENV_LOG_DIR, &dir);
        std::env::remove_var(ENV_LOG_DISABLED);

        let sid = "jsonl-shape";
        {
            let mut log = EventLog::new(sid).expect("open log");
            log.append(&ev(1, EventKind::SessionStart)).expect("a");
            log.append(&ev(2, EventKind::DeckPlay { deck: DeckId::A }))
                .expect("a");
            log.flush().expect("flush");
        }

        let path = dir.join(sid).join("events.jsonl");
        let file = File::open(&path).expect("open");
        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .collect::<std::io::Result<_>>()
            .expect("lines");
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid json");
            assert!(v.get("id").is_some(), "id present: {line}");
            assert!(v.get("ts_micros").is_some(), "ts_micros present: {line}");
            assert!(v.get("source").is_some(), "source present: {line}");
            assert!(v.get("kind").is_some(), "kind present: {line}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_id_format_iso_timestamp_plus_hex_suffix() {
        let sid = new_session_id();
        // Format: 20260518T013312Z-a4f2 → 21 chars exactly.
        assert_eq!(sid.len(), 21, "len = {}: {}", sid.len(), sid);
        assert_eq!(&sid[8..9], "T");
        assert_eq!(&sid[15..16], "Z");
        assert_eq!(&sid[16..17], "-");
        // Last 4 chars are lowercase hex.
        let suffix = &sid[17..];
        assert_eq!(suffix.len(), 4);
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "suffix not lowercase hex: {suffix}"
        );
        // Leading 4 chars should be a 4-digit year >= 2026.
        let year: i32 = sid[..4].parse().expect("year parse");
        assert!(year >= 2026, "year = {year}");
    }

    #[test]
    fn disabled_mode_creates_no_file() {
        let _g = test_lock();
        let _env = EnvGuard::new();
        let dir = scratch_dir("disabled");
        std::env::set_var(ENV_LOG_DIR, &dir);
        std::env::set_var(ENV_LOG_DISABLED, "1");

        let sid = "should-not-appear";
        {
            let mut log = EventLog::new(sid).expect("disabled open");
            assert!(log.is_disabled());
            assert!(log.path().is_none());
            // Append should be a silent no-op.
            log.append(&ev(1, EventKind::SessionStart)).expect("noop");
        }

        let dir_for_sid = dir.join(sid);
        assert!(
            !dir_for_sid.exists(),
            "disabled mode created dir at {}",
            dir_for_sid.display()
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drop_flushes_partial_buffer_under_flush_threshold() {
        let _g = test_lock();
        let _env = EnvGuard::new();
        let dir = scratch_dir("dropflush");
        std::env::set_var(ENV_LOG_DIR, &dir);
        std::env::remove_var(ENV_LOG_DISABLED);

        let sid = "drop-flush";
        // 5 events << FLUSH_EVERY_N (100) so we depend on Drop to flush.
        {
            let mut log = EventLog::new(sid).expect("open");
            for i in 0..5u64 {
                log.append(&ev(i, EventKind::DeckPlay { deck: DeckId::A }))
                    .expect("append");
            }
        } // Drop here.

        let path = dir.join(sid).join("events.jsonl");
        let file = File::open(&path).expect("open after drop");
        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .collect::<std::io::Result<_>>()
            .expect("lines");
        assert_eq!(lines.len(), 5, "drop should flush all 5 events");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_log_dir_override_takes_priority() {
        let _g = test_lock();
        let _env = EnvGuard::new();
        let dir = scratch_dir("override");
        std::env::set_var(ENV_LOG_DIR, &dir);
        std::env::remove_var(ENV_LOG_DISABLED);

        let sid = "override-sid";
        let log = EventLog::new(sid).expect("open");
        let path = log.path().expect("path").to_path_buf();
        drop(log);
        // Path must be under the override dir, NOT under XDG/HOME.
        assert!(
            path.starts_with(&dir),
            "path {path:?} not under override {dir:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_appends_to_existing_file() {
        let _g = test_lock();
        let _env = EnvGuard::new();
        let dir = scratch_dir("reopen");
        std::env::set_var(ENV_LOG_DIR, &dir);
        std::env::remove_var(ENV_LOG_DISABLED);

        let sid = "reopen-sid";
        {
            let mut log = EventLog::new(sid).expect("first open");
            log.append(&ev(1, EventKind::SessionStart)).expect("a1");
        }
        {
            let mut log = EventLog::new(sid).expect("second open");
            log.append(&ev(2, EventKind::SessionEnd)).expect("a2");
        }
        let path = dir.join(sid).join("events.jsonl");
        let file = File::open(&path).expect("open");
        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .collect::<std::io::Result<_>>()
            .expect("lines");
        assert_eq!(lines.len(), 2, "O_APPEND should preserve prior content");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unix_secs_to_utc_known_anchor() {
        // 2026-05-18T01:33:12Z → 1779067992 (verified against
        // python `calendar.timegm(datetime.datetime(2026,5,18,1,33,12).timetuple())`).
        let (y, mo, d, h, mi, s) = unix_secs_to_utc(1779067992);
        assert_eq!((y, mo, d, h, mi, s), (2026, 5, 18, 1, 33, 12));
        // Epoch.
        assert_eq!(unix_secs_to_utc(0), (1970, 1, 1, 0, 0, 0));
        // 2000-03-01 — exercises the Hinnant algorithm's century /
        // quad-century branches.
        assert_eq!(unix_secs_to_utc(951868800), (2000, 3, 1, 0, 0, 0));
    }
}
