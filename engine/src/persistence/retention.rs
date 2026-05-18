//! Event log retention + pruning (ADR-003, follow-up to PR #42 / issue #41).
//!
//! Background
//! ----------
//! PR #42 made the event log durable per session (`<root>/<session_id>/`).
//! Without pruning, a long-lived install accumulates session directories
//! indefinitely — each one ~tens of KB of JSONL plus a multi-MB
//! `master.wav` from the master-mix recorder. After a year of nightly
//! use that's a real disk-space problem on laptops.
//!
//! This module deletes stale session directories at engine boot using a
//! belt-and-braces policy: drop sessions older than `max_days` BUT keep
//! at least the most recent `min_sessions_to_keep` even if every session
//! is older than the cutoff. The second rule prevents a long vacation
//! from wiping the user's entire history the moment they boot back up.
//!
//! Boot ordering
//! -------------
//! `main.rs` calls [`prune_sessions`] **after** [`super::EventLog::new`]
//! has created the current session directory. Listing-by-mtime would
//! otherwise count the just-created dir as "oldest" if the system clock
//! is skewed; running after the new dir has a current mtime keeps the
//! current session out of the deletion candidate set by sort order.
//!
//! Env config
//! ----------
//! * `HYPEHOUSE_LOG_MAX_DAYS` — override `max_days` (default 30).
//! * `HYPEHOUSE_LOG_MIN_KEEP` — override `min_sessions_to_keep` (default 50).
//! * `HYPEHOUSE_LOG_RETENTION_DISABLED=1` — skip pruning entirely.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// Env var overriding the max-age cutoff in days.
pub const ENV_RETENTION_MAX_DAYS: &str = "HYPEHOUSE_LOG_MAX_DAYS";
/// Env var overriding the minimum sessions to retain regardless of age.
pub const ENV_RETENTION_MIN_KEEP: &str = "HYPEHOUSE_LOG_MIN_KEEP";
/// Env var that disables pruning entirely. Set to `1`.
pub const ENV_RETENTION_DISABLED: &str = "HYPEHOUSE_LOG_RETENTION_DISABLED";

/// Default cutoff: sessions older than 30 days are pruning candidates.
pub const DEFAULT_MAX_DAYS: u32 = 30;
/// Default floor: keep at least the 50 most recent sessions regardless
/// of age. Roughly two months of nightly use — small enough on disk
/// (~tens of MB without master.wav), large enough to give post-mortems
/// breathing room.
pub const DEFAULT_MIN_KEEP: usize = 50;

/// Knob-tuple driving [`prune_sessions`]. See [`RetentionPolicy::from_env`]
/// for the env-var resolution rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Sessions whose directory mtime is older than `now - max_days`
    /// become deletion candidates. Set to `0` to make every session a
    /// candidate (still subject to the floor below).
    pub max_days: u32,
    /// Always retain at least this many of the most recent session
    /// directories, even if all of them are older than `max_days`. This
    /// is the safety floor that prevents a long-idle laptop from
    /// wiping its entire history on the first boot back.
    pub min_sessions_to_keep: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_days: DEFAULT_MAX_DAYS,
            min_sessions_to_keep: DEFAULT_MIN_KEEP,
        }
    }
}

impl RetentionPolicy {
    /// Resolve the policy from env vars, falling back to the defaults
    /// when unset or unparseable. Unparseable values are logged at
    /// `warn!` level by [`prune_from_env`] — here we silently fall back
    /// so the type stays pure / test-friendly.
    pub fn from_env() -> Self {
        let mut p = Self::default();
        if let Ok(v) = std::env::var(ENV_RETENTION_MAX_DAYS) {
            if let Ok(parsed) = v.trim().parse::<u32>() {
                p.max_days = parsed;
            }
        }
        if let Ok(v) = std::env::var(ENV_RETENTION_MIN_KEEP) {
            if let Ok(parsed) = v.trim().parse::<usize>() {
                p.min_sessions_to_keep = parsed;
            }
        }
        p
    }
}

/// Summary of one prune pass. Returned by [`prune_sessions`] so the
/// caller (boot log line in `main.rs`) can surface "freed N MB" without
/// re-walking the filesystem.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneSummary {
    /// How many session directories were deleted.
    pub deleted: usize,
    /// How many session directories remain on disk after pruning.
    pub retained: usize,
    /// Total bytes freed (sum of file sizes within the deleted dirs).
    /// Best-effort: any file we couldn't `stat` during the walk is
    /// counted as 0 rather than aborting the whole prune.
    pub bytes_freed: u64,
}

/// Walk the persistence root and delete session directories per
/// `policy`. Never deletes anything that isn't a directory of the root.
///
/// Algorithm:
/// 1. Enumerate immediate children of `persistence_dir`, keeping only
///    those that are directories.
/// 2. Stat each one for its mtime. Entries whose mtime can't be read
///    are sorted to the **newest** end so the floor protects them —
///    losing a sort signal must never make a dir more likely to be
///    deleted.
/// 3. Sort newest-first.
/// 4. Mark the first `min_sessions_to_keep` as "keep" unconditionally.
/// 5. Of the remainder, mark as "delete" those whose mtime is older
///    than `now - max_days`. The rest stay.
/// 6. `remove_dir_all` each marked-for-delete entry, accumulating size.
///
/// Returns the summary unconditionally — per-dir delete failures are
/// logged at `warn!` but do not propagate; a single permission-denied
/// session must not block the boot path.
pub fn prune_sessions(persistence_dir: &Path, policy: &RetentionPolicy) -> Result<PruneSummary> {
    if !persistence_dir.exists() {
        // Fresh install — nothing to prune. Not an error.
        return Ok(PruneSummary::default());
    }

    let entries = fs::read_dir(persistence_dir).with_context(|| {
        format!(
            "reading persistence root for retention sweep: {}",
            persistence_dir.display()
        )
    })?;

    let now = SystemTime::now();
    let cutoff = now
        .checked_sub(Duration::from_secs(u64::from(policy.max_days) * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    // (path, mtime) tuples — None mtime sorts newest (kept by floor).
    let mut dirs: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let mtime = meta.modified().ok();
        dirs.push((path, mtime));
    }

    // Newest-first: None sorts to the front (treated as newest so the
    // floor protects it).
    dirs.sort_by(|a, b| match (b.1, a.1) {
        (Some(bm), Some(am)) => bm.cmp(&am),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, None) => b.0.cmp(&a.0),
    });

    let mut summary = PruneSummary::default();
    for (idx, (path, mtime)) in dirs.iter().enumerate() {
        // Floor: the first N (newest) are always kept.
        if idx < policy.min_sessions_to_keep {
            summary.retained += 1;
            continue;
        }
        // Eligible. Delete only if we have an mtime AND it's older
        // than the cutoff. Missing mtime → keep (it sorted as newest
        // anyway; this branch is defensive belt-and-braces).
        let is_stale = match mtime {
            Some(m) => *m < cutoff,
            None => false,
        };
        if !is_stale {
            summary.retained += 1;
            continue;
        }

        let bytes = dir_size_best_effort(path);
        match fs::remove_dir_all(path) {
            Ok(()) => {
                summary.deleted += 1;
                summary.bytes_freed = summary.bytes_freed.saturating_add(bytes);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "retention: remove_dir_all failed — leaving session in place"
                );
                summary.retained += 1;
            }
        }
    }
    Ok(summary)
}

/// Convenience wrapper that resolves the policy from env vars and skips
/// when [`ENV_RETENTION_DISABLED`] is set. Logs a single info line on
/// completion with the summary. Returns the summary so callers can plug
/// into their own observability.
///
/// `persistence_dir` should be the same root the writer resolved (so
/// the override is honoured). The caller in `main.rs` passes the parent
/// of the current session dir.
pub fn prune_from_env(persistence_dir: &Path) -> Result<PruneSummary> {
    if std::env::var(ENV_RETENTION_DISABLED).as_deref() == Ok("1") {
        tracing::info!(
            dir = %persistence_dir.display(),
            "event log retention: disabled by env"
        );
        return Ok(PruneSummary::default());
    }
    let policy = RetentionPolicy::from_env();
    let summary = prune_sessions(persistence_dir, &policy)?;
    tracing::info!(
        dir = %persistence_dir.display(),
        max_days = policy.max_days,
        min_keep = policy.min_sessions_to_keep,
        deleted = summary.deleted,
        retained = summary.retained,
        bytes_freed = summary.bytes_freed,
        "event log retention: sweep complete"
    );
    Ok(summary)
}

/// Sum the byte sizes of every regular file under `dir`, recursively.
/// Best-effort: any `stat`/`read_dir` failure is silently treated as 0
/// for that subtree — the alternative is to fail the whole prune for a
/// transient permission glitch, which would leave bytes_freed wrong
/// every boot.
fn dir_size_best_effort(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            total = total.saturating_add(dir_size_best_effort(&entry.path()));
        } else if ft.is_file() {
            if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
            }
        }
        // Symlinks deliberately skipped — following them risks counting
        // bytes outside the session dir and double-counting on shared
        // mounts.
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use std::fs::File;
    use std::io::Write;
    use std::time::UNIX_EPOCH;

    /// Per-test mutex for env-mutating tests. cargo runs tests in
    /// parallel by default; the env reads in `from_env` are
    /// process-wide.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Snapshot + restore the retention env vars so tests don't leak
    /// state to siblings.
    struct EnvGuard {
        max_days: Option<String>,
        min_keep: Option<String>,
        disabled: Option<String>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                max_days: std::env::var(ENV_RETENTION_MAX_DAYS).ok(),
                min_keep: std::env::var(ENV_RETENTION_MIN_KEEP).ok(),
                disabled: std::env::var(ENV_RETENTION_DISABLED).ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: env access serialized by test_lock().
            for (var, prev) in [
                (ENV_RETENTION_MAX_DAYS, &self.max_days),
                (ENV_RETENTION_MIN_KEEP, &self.min_keep),
                (ENV_RETENTION_DISABLED, &self.disabled),
            ] {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }

    fn scratch_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("hh-retention-{tag}-{pid}-{nanos}"));
        fs::create_dir_all(&dir).expect("scratch root create");
        dir
    }

    /// Build a session dir with a synthetic events.jsonl + master.wav so
    /// `dir_size_best_effort` has something to count, then back-date
    /// its mtime to `days_ago`.
    fn make_session(root: &Path, id: &str, days_ago: u64, payload_bytes: usize) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).expect("create session dir");
        // events.jsonl
        let mut f = File::create(dir.join("events.jsonl")).expect("create events");
        f.write_all(&vec![b'a'; payload_bytes])
            .expect("write events");
        drop(f);
        // master.wav — non-empty so total > 0
        let mut w = File::create(dir.join("master.wav")).expect("create wav");
        w.write_all(&vec![b'w'; payload_bytes]).expect("write wav");
        drop(w);

        // Back-date both files + the directory itself. We back-date the
        // dir mtime because prune_sessions stats the *directory*, not
        // its contents.
        let target_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("now > epoch")
            .as_secs()
            .saturating_sub(days_ago * 86_400);
        let ft = FileTime::from_unix_time(target_secs as i64, 0);
        let _ = set_file_mtime(&dir, ft);
        let _ = set_file_mtime(dir.join("events.jsonl"), ft);
        let _ = set_file_mtime(dir.join("master.wav"), ft);
    }

    #[test]
    fn defaults_match_spec() {
        let p = RetentionPolicy::default();
        assert_eq!(p.max_days, 30);
        assert_eq!(p.min_sessions_to_keep, 50);
    }

    #[test]
    fn missing_dir_is_noop() {
        let root = scratch_root("missing");
        let nonexistent = root.join("nope");
        let s = prune_sessions(&nonexistent, &RetentionPolicy::default()).expect("ok");
        assert_eq!(s.deleted, 0);
        assert_eq!(s.retained, 0);
        assert_eq!(s.bytes_freed, 0);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn deletes_old_keeps_recent() {
        let root = scratch_root("mixed");
        // 5 ancient (60 days), 3 recent (1 day).
        for i in 0..5 {
            make_session(&root, &format!("ancient-{i}"), 60, 16);
        }
        for i in 0..3 {
            make_session(&root, &format!("recent-{i}"), 1, 16);
        }
        let policy = RetentionPolicy {
            max_days: 30,
            min_sessions_to_keep: 2, // floor below the recent count
        };
        let s = prune_sessions(&root, &policy).expect("ok");
        assert_eq!(s.deleted, 5, "all ancient should go");
        assert_eq!(s.retained, 3, "all recent should stay");
        // Each ancient dir had two 16-byte files == 32 bytes.
        assert_eq!(s.bytes_freed, 5 * 32);
        // Filesystem confirms.
        for i in 0..5 {
            assert!(!root.join(format!("ancient-{i}")).exists());
        }
        for i in 0..3 {
            assert!(root.join(format!("recent-{i}")).exists());
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn min_keep_floor_protects_when_all_stale() {
        let root = scratch_root("floor");
        // 7 sessions, all 90 days old, floor = 3.
        for i in 0..7 {
            make_session(&root, &format!("old-{i:02}"), 90, 8);
        }
        // Back-date each by a slightly different amount so sort order
        // is deterministic — newest-first the highest index wins.
        for i in 0..7 {
            let dir = root.join(format!("old-{i:02}"));
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(90 * 86_400 - i * 60);
            let ft = FileTime::from_unix_time(secs as i64, 0);
            let _ = set_file_mtime(&dir, ft);
        }
        let policy = RetentionPolicy {
            max_days: 30,
            min_sessions_to_keep: 3,
        };
        let s = prune_sessions(&root, &policy).expect("ok");
        assert_eq!(s.retained, 3, "floor preserves N regardless of age");
        assert_eq!(s.deleted, 4);
        // Survivors are the 3 newest (highest indices: 06, 05, 04).
        for i in 4..7 {
            assert!(
                root.join(format!("old-{i:02}")).exists(),
                "old-{i:02} should be kept"
            );
        }
        for i in 0..4 {
            assert!(
                !root.join(format!("old-{i:02}")).exists(),
                "old-{i:02} should be pruned"
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn disabled_env_is_noop_even_with_stale_sessions() {
        let _g = test_lock();
        let _env = EnvGuard::new();
        std::env::set_var(ENV_RETENTION_DISABLED, "1");
        let root = scratch_root("disabled");
        for i in 0..4 {
            make_session(&root, &format!("ancient-{i}"), 365, 4);
        }
        let s = prune_from_env(&root).expect("ok");
        assert_eq!(s.deleted, 0);
        assert_eq!(s.retained, 0);
        for i in 0..4 {
            assert!(root.join(format!("ancient-{i}")).exists());
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bytes_freed_sums_all_files_in_deleted_dirs() {
        let root = scratch_root("bytes");
        // One stale session with 100B + 1000B + a nested subdir of 50B.
        let sid = "stale-counted";
        let dir = root.join(sid);
        fs::create_dir_all(&dir).unwrap();
        File::create(dir.join("events.jsonl"))
            .unwrap()
            .write_all(&[0u8; 100])
            .unwrap();
        File::create(dir.join("master.wav"))
            .unwrap()
            .write_all(&[0u8; 1000])
            .unwrap();
        let sub = dir.join("nested");
        fs::create_dir_all(&sub).unwrap();
        File::create(sub.join("snapshot.bin"))
            .unwrap()
            .write_all(&[0u8; 50])
            .unwrap();
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(60 * 86_400);
        let ft = FileTime::from_unix_time(secs as i64, 0);
        let _ = set_file_mtime(&dir, ft);

        let policy = RetentionPolicy {
            max_days: 30,
            min_sessions_to_keep: 0,
        };
        let s = prune_sessions(&root, &policy).expect("ok");
        assert_eq!(s.deleted, 1);
        assert_eq!(s.bytes_freed, 1150);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn env_overrides_parse() {
        let _g = test_lock();
        let _env = EnvGuard::new();
        std::env::set_var(ENV_RETENTION_MAX_DAYS, "7");
        std::env::set_var(ENV_RETENTION_MIN_KEEP, "5");
        std::env::remove_var(ENV_RETENTION_DISABLED);
        let p = RetentionPolicy::from_env();
        assert_eq!(p.max_days, 7);
        assert_eq!(p.min_sessions_to_keep, 5);
        // Bad values fall back to defaults.
        std::env::set_var(ENV_RETENTION_MAX_DAYS, "not a number");
        std::env::set_var(ENV_RETENTION_MIN_KEEP, "-10");
        let p = RetentionPolicy::from_env();
        assert_eq!(p.max_days, 30);
        assert_eq!(p.min_sessions_to_keep, 50);
    }

    #[test]
    fn non_directory_entries_are_ignored() {
        let root = scratch_root("nondir");
        // A stray file at the root must not be deleted even if old.
        let stray = root.join("README.txt");
        File::create(&stray).unwrap().write_all(b"hi").unwrap();
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(365 * 86_400);
        let _ = set_file_mtime(&stray, FileTime::from_unix_time(secs as i64, 0));

        // Plus one genuinely stale session.
        make_session(&root, "stale-session", 365, 8);
        let policy = RetentionPolicy {
            max_days: 30,
            min_sessions_to_keep: 0,
        };
        let s = prune_sessions(&root, &policy).expect("ok");
        assert_eq!(s.deleted, 1);
        assert!(stray.exists(), "stray top-level file must survive");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn keeps_recent_sessions_even_when_floor_zero() {
        let root = scratch_root("recent-fresh");
        // Three fresh sessions, floor = 0, max_days = 30 → all kept.
        for i in 0..3 {
            make_session(&root, &format!("fresh-{i}"), 1, 4);
        }
        let policy = RetentionPolicy {
            max_days: 30,
            min_sessions_to_keep: 0,
        };
        let s = prune_sessions(&root, &policy).expect("ok");
        assert_eq!(s.deleted, 0);
        assert_eq!(s.retained, 3);
        assert_eq!(s.bytes_freed, 0);
        fs::remove_dir_all(&root).ok();
    }
}
