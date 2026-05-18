//! Read-only enumeration + replay of past session directories.
//!
//! Companion to [`super::EventLog`] (writer) and [`super::replay`] (event
//! parser / state fold). The bridge layer exposes two RPCs that drive
//! this module:
//!
//! * `engine.list_sessions` — enumerate session directories under the
//!   resolved persistence root and return a lightweight summary per
//!   session (id, started/ended timestamps, event count, master.wav
//!   presence + size).
//! * `engine.replay_session` — fold the `events.jsonl` of a single
//!   session through `replay_state` and return the resulting
//!   `EngineState` snapshot.
//!
//! Resolution of the persistence root follows the same priority order
//! the [`super::EventLog`] writer uses:
//!
//!   1. `$HYPEHOUSE_EVENT_LOG_DIR`
//!   2. `$XDG_DATA_HOME/hypehouse-live/sessions`
//!   3. `$HOME/.local/share/hypehouse-live/sessions`
//!
//! No state is mutated by this module — it never touches the live
//! engine. v0.1 deliberately does **not** support "load this snapshot
//! into the running engine"; the snapshot is read-only and the UI shows
//! it as such. A future PR can layer a `engine.load_session_into_engine`
//! event on top of this primitive.
//!
//! Threading
//! ---------
//! Called from the WS server's tokio task (control-thread peer). All I/O
//! is `std::fs::*` blocking but the result sets are small (≤50 sessions
//! by default; tens of KB for a typical events.jsonl), so we don't
//! `spawn_blocking`. If a future session grows past a few MB of events
//! the dispatch handler can move to `tokio::task::spawn_blocking` in
//! one line.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::{EngineState, Event};

use super::replay;

/// Maximum number of sessions returned by `list_sessions`. Sorted by
/// start time desc, the most recent N are surfaced; older ones are
/// pruned client-side until/unless we add pagination.
pub const LIST_SESSIONS_DEFAULT_LIMIT: usize = 50;

/// Filename for the per-session master mix recording (matches the
/// constant in `engine/src/main.rs::resolve_recording_path`).
pub const MASTER_WAV_FILENAME: &str = "master.wav";

/// Lightweight summary of one persisted session — no events or audio
/// payload, just metadata cheap enough to fetch for every directory.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Directory name (matches the id minted by
    /// [`super::new_session_id`], e.g. `20260518T013312Z-a4f2`).
    pub id: String,
    /// `ts_micros` of the first event in `events.jsonl`. `None` when
    /// the log is empty or unreadable; the UI sorts these to the bottom.
    pub started_at_micros: Option<i64>,
    /// `ts_micros` of the last event in `events.jsonl`. `None` when
    /// empty. May equal `started_at_micros` for one-event logs.
    pub ended_at_micros: Option<i64>,
    /// Number of JSONL lines (== events). 0 when missing or empty.
    pub event_count: u64,
    /// True when `master.wav` exists in the session dir AND is
    /// non-empty. Empty recordings (zero-byte files from a startup
    /// abort) are reported as `false` so the UI doesn't offer a
    /// useless download.
    pub has_recording: bool,
    /// Byte size of `master.wav`. `None` when absent.
    pub recording_size_bytes: Option<u64>,
}

/// Wire envelope for `engine.list_sessions` results. Object-typed so we
/// can add future top-level fields (e.g. `total`, `pagination_token`)
/// without breaking existing TS clients.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ListSessionsResult {
    pub sessions: Vec<SessionSummary>,
}

/// Wire envelope for `engine.replay_session` results.
///
/// `state` is the in-memory fold of every event in `events.jsonl`. The
/// audio recording is **not** consulted — see module docs.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReplaySessionResult {
    pub state: EngineState,
    /// How many events were replayed. Useful for the UI to display
    /// "replayed N events" without re-counting client-side.
    pub event_count: u64,
}

/// Resolve the persistence root the same way `EventLog::new` does. Kept
/// duplicated (not exported) from `mod.rs` deliberately: the writer
/// signals errors via `Result<EventLog>` while the reader needs the
/// path even when nothing has been written yet, so its missing-root
/// behaviour is to return an empty list (the user simply has no past
/// sessions yet).
fn resolve_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(super::ENV_LOG_DIR) {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return Some(PathBuf::from(xdg).join("hypehouse-live").join("sessions"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return Some(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("hypehouse-live")
                    .join("sessions"),
            );
        }
    }
    None
}

/// Enumerate session directories under the resolved root.
///
/// Returns `Ok(vec![])` when the root cannot be resolved or does not
/// exist yet — that's the "user has no past sessions" state, not an
/// error. Per-directory I/O errors (a session dir whose `events.jsonl`
/// is unreadable, malformed, or fails to open) are tolerated: the
/// summary still appears with `event_count = 0` + `None` timestamps so
/// the UI can show the directory exists without crashing.
///
/// Sorts the result by `started_at_micros` descending (most recent
/// first), with `None` started_at sorted to the end. Truncates to
/// `LIST_SESSIONS_DEFAULT_LIMIT` (50).
pub fn list_sessions() -> Result<Vec<SessionSummary>> {
    let Some(root) = resolve_root() else {
        return Ok(Vec::new());
    };
    list_sessions_in(&root)
}

/// Same as [`list_sessions`] but reads from a caller-supplied root.
/// Exposed so tests don't have to mutate process env to redirect I/O.
pub fn list_sessions_in(root: &Path) -> Result<Vec<SessionSummary>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            // Permission denied / IO error on the root itself is the
            // one case we propagate — the user can't tell why their
            // history is empty otherwise.
            return Err(
                anyhow::Error::new(e).context(format!("reading sessions root {}", root.display()))
            );
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        out.push(summarize_one(&path, id));
    }
    out.sort_by(|a, b| match (b.started_at_micros, a.started_at_micros) {
        (Some(bv), Some(av)) => bv.cmp(&av),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.id.cmp(&a.id),
    });
    out.truncate(LIST_SESSIONS_DEFAULT_LIMIT);
    Ok(out)
}

/// Build a single [`SessionSummary`] from a directory path. Never
/// returns an error — any read failure degrades to "unknown timestamp,
/// zero events, no recording".
fn summarize_one(dir: &Path, id: String) -> SessionSummary {
    let events_path = dir.join("events.jsonl");
    let (started, ended, count) = scan_events_meta(&events_path).unwrap_or((None, None, 0));
    let recording = dir.join(MASTER_WAV_FILENAME);
    let (has_recording, size) = match fs::metadata(&recording) {
        Ok(m) if m.is_file() && m.len() > 0 => (true, Some(m.len())),
        Ok(m) if m.is_file() => (false, Some(m.len())),
        _ => (false, None),
    };
    SessionSummary {
        id,
        started_at_micros: started,
        ended_at_micros: ended,
        event_count: count,
        has_recording,
        recording_size_bytes: size,
    }
}

/// Scan `events.jsonl` for the first/last event timestamps + total
/// line count. Reads sequentially — the file is append-only JSONL and
/// for our session sizes (a few thousand events) the linear scan is
/// negligible compared to the round-trip RPC latency.
fn scan_events_meta(path: &Path) -> Result<(Option<i64>, Option<i64>, u64)> {
    let file = File::open(path)
        .with_context(|| format!("opening events log {} for summary", path.display()))?;
    let reader = BufReader::new(file);
    let mut count: u64 = 0;
    let mut first: Option<i64> = None;
    let mut last: Option<i64> = None;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // Bad UTF-8 / partial — skip without aborting the whole scan.
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        count += 1;
        // Cheap field-only deserialize — full Event parse would allocate
        // the whole struct just to read one field.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(ts) = v.get("ts_micros").and_then(serde_json::Value::as_i64) {
                if first.is_none() {
                    first = Some(ts);
                }
                last = Some(ts);
            }
        }
    }
    Ok((first, last, count))
}

/// Replay the events of a single session and return the resulting
/// `EngineState` plus the event count.
///
/// `session_id` must be a directory name as returned by
/// [`list_sessions`]; the function rejects ids containing path
/// separators (`/`, `\`) or leading dots to keep the read confined to
/// the persistence root (no `..`-escape, no absolute paths).
///
/// Returns `Err` only when the session id is invalid or the root
/// cannot be resolved. A missing or empty `events.jsonl` returns an
/// `EngineState::default()` with `event_count = 0` — the UI can render
/// that as "no events recorded yet" without raising an alarm.
pub fn replay_session(session_id: &str) -> Result<ReplaySessionResult> {
    let root = resolve_root().context("could not resolve persistence root")?;
    replay_session_in(&root, session_id)
}

/// Same as [`replay_session`] but reads from a caller-supplied root.
pub fn replay_session_in(root: &Path, session_id: &str) -> Result<ReplaySessionResult> {
    validate_session_id(session_id)?;
    let dir = root.join(session_id);
    if !dir.is_dir() {
        anyhow::bail!("session not found: {session_id}");
    }
    let events_path = dir.join("events.jsonl");
    if !events_path.exists() {
        return Ok(ReplaySessionResult {
            state: EngineState::default(),
            event_count: 0,
        });
    }
    let mut reader = replay::EventReplay::open(&events_path)
        .with_context(|| format!("opening events log for replay: {}", events_path.display()))?;
    let events: Vec<Event> = reader.collect().context("parsing event log for replay")?;
    let count = events.len() as u64;
    let state = replay::replay_state(&events);
    Ok(ReplaySessionResult {
        state,
        event_count: count,
    })
}

/// Reject session ids that look like path traversal attempts. The
/// persistence layer mints ids of the form `YYYYMMDDTHHMMSSZ-XXXX`
/// (21 chars, ASCII alphanumeric + `T`/`Z`/`-`). We accept any string
/// that doesn't introduce a path separator or a leading dot so manually
/// minted ids stay usable while keeping the read confined.
fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() {
        anyhow::bail!("session id is empty");
    }
    if session_id.len() > 128 {
        anyhow::bail!("session id too long");
    }
    if session_id.starts_with('.') {
        anyhow::bail!("session id starts with '.'");
    }
    if session_id.contains(['/', '\\', '\0']) {
        anyhow::bail!("session id contains path separators");
    }
    Ok(())
}

/// Resolve the on-disk path to `master.wav` for a given session id,
/// performing the same id validation as [`replay_session`]. Returns
/// `Err` when the id is invalid or the root cannot be resolved; the
/// returned path is **not** checked for existence — the caller (the
/// HTTP endpoint future PR) decides how to render "not found".
#[allow(dead_code)] // Reserved for the master.wav HTTP endpoint.
pub fn recording_path_for(session_id: &str) -> Result<PathBuf> {
    validate_session_id(session_id)?;
    let root = resolve_root().context("could not resolve persistence root")?;
    Ok(root.join(session_id).join(MASTER_WAV_FILENAME))
}

/// Microsecond-precision unix epoch helper. Used by tests to mint
/// deterministic event timestamps without pulling chrono.
#[cfg(test)]
fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeckId, EventKind, EventSource, TrackRef};
    use std::fs::File;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("hh-sessions-{tag}-{pid}-{nanos}"));
        fs::create_dir_all(&dir).expect("scratch root create");
        dir
    }

    fn write_jsonl(path: &Path, events: &[Event]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        let mut f = File::create(path).expect("create jsonl");
        for e in events {
            let line = serde_json::to_string(e).expect("encode");
            writeln!(f, "{line}").expect("write line");
        }
    }

    fn ev(id: u64, kind: EventKind) -> Event {
        Event {
            id,
            ts_micros: now_micros() + id as i64,
            source: EventSource::Ui,
            kind,
        }
    }

    #[test]
    fn list_sessions_returns_empty_when_root_missing() {
        let root = scratch_root("missing");
        // Don't create any session dirs.
        let out = list_sessions_in(&root).expect("list ok");
        assert!(out.is_empty());
        let nonexistent = root.join("does-not-exist");
        let out = list_sessions_in(&nonexistent).expect("list ok on missing");
        assert!(out.is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_sessions_reads_directories_and_sorts_desc() {
        let root = scratch_root("sort");
        // Three sessions with deliberately staggered timestamps.
        let sid_old = "20260101T000000Z-aaaa";
        let sid_mid = "20260201T000000Z-bbbb";
        let sid_new = "20260301T000000Z-cccc";
        for (sid, ts) in [
            (sid_old, 1_000_000i64),
            (sid_mid, 2_000_000),
            (sid_new, 3_000_000),
        ] {
            let path = root.join(sid).join("events.jsonl");
            let e = Event {
                id: 1,
                ts_micros: ts,
                source: EventSource::Ui,
                kind: EventKind::SessionStart,
            };
            write_jsonl(&path, &[e]);
        }
        let out = list_sessions_in(&root).expect("list ok");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].id, sid_new);
        assert_eq!(out[1].id, sid_mid);
        assert_eq!(out[2].id, sid_old);
        assert_eq!(out[0].started_at_micros, Some(3_000_000));
        assert_eq!(out[0].event_count, 1);
        assert!(!out[0].has_recording);
        assert_eq!(out[0].recording_size_bytes, None);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_sessions_picks_up_master_wav_size() {
        let root = scratch_root("recording");
        let sid = "20260301T010101Z-dddd";
        let dir = root.join(sid);
        write_jsonl(&dir.join("events.jsonl"), &[ev(1, EventKind::SessionStart)]);
        let wav = dir.join(MASTER_WAV_FILENAME);
        fs::write(&wav, b"RIFF????WAVEfmt fake bytes").expect("write wav");
        let out = list_sessions_in(&root).expect("list ok");
        assert_eq!(out.len(), 1);
        assert!(out[0].has_recording);
        assert_eq!(
            out[0].recording_size_bytes,
            Some(fs::metadata(&wav).unwrap().len())
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_sessions_tolerates_corrupt_events_file() {
        let root = scratch_root("corrupt");
        let sid = "20260301T020202Z-eeee";
        let path = root.join(sid).join("events.jsonl");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(&path, b"not json at all\n{also not json\n").expect("write corrupt");
        let out = list_sessions_in(&root).expect("list still ok");
        assert_eq!(out.len(), 1, "corrupt session still listed");
        assert_eq!(out[0].id, sid);
        // Count picks up 2 non-empty lines even though they don't parse.
        assert_eq!(out[0].event_count, 2);
        // Timestamps are None because none of the lines deserialized.
        assert!(out[0].started_at_micros.is_none());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_sessions_caps_at_50_sessions() {
        let root = scratch_root("cap");
        for i in 0..55 {
            let sid = format!("20260101T00{i:02}00Z-cap{i}");
            let path = root.join(&sid).join("events.jsonl");
            let e = Event {
                id: 1,
                ts_micros: 1_000_000 + i as i64,
                source: EventSource::Ui,
                kind: EventKind::SessionStart,
            };
            write_jsonl(&path, &[e]);
        }
        let out = list_sessions_in(&root).expect("list ok");
        assert_eq!(out.len(), LIST_SESSIONS_DEFAULT_LIMIT);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_sessions_skips_non_directory_entries() {
        let root = scratch_root("skip");
        // Stray file at the root — must be ignored.
        fs::write(root.join("README.md"), b"hello").expect("stray file");
        let out = list_sessions_in(&root).expect("list ok");
        assert!(out.is_empty(), "non-dir entries must not surface");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn replay_session_returns_default_state_on_empty_log() {
        let root = scratch_root("empty");
        let sid = "20260301T030303Z-ffff";
        let dir = root.join(sid);
        fs::create_dir_all(&dir).expect("dir");
        // No events.jsonl at all.
        let out = replay_session_in(&root, sid).expect("ok");
        assert_eq!(out.event_count, 0);
        assert!(!out.state.session_active);

        // Now empty events.jsonl.
        fs::write(dir.join("events.jsonl"), b"").expect("empty file");
        let out = replay_session_in(&root, sid).expect("ok");
        assert_eq!(out.event_count, 0);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn replay_session_reconstructs_known_state() {
        let root = scratch_root("replay-known");
        let sid = "20260301T040404Z-1111";
        let events = vec![
            ev(1, EventKind::SessionStart),
            ev(
                2,
                EventKind::DeckLoad {
                    deck: DeckId::A,
                    track: TrackRef {
                        id: "t1".into(),
                        path: "/m/t1.mp3".into(),
                    },
                    bpm: 128.0,
                    beat_grid_anchor_ms: 0,
                    downbeats_ms: vec![0, 1875, 3750],
                    hot_cues: [None; 8],
                    track_gain_db: 0.0,
                },
            ),
            ev(3, EventKind::DeckPlay { deck: DeckId::A }),
            ev(4, EventKind::Crossfader { value: 0.75 }),
        ];
        write_jsonl(&root.join(sid).join("events.jsonl"), &events);
        let out = replay_session_in(&root, sid).expect("ok");
        assert_eq!(out.event_count, 4);
        assert!(out.state.session_active);
        assert!(out.state.deck_a.playing);
        assert_eq!(out.state.deck_a.bpm, 128.0);
        assert!((out.state.crossfader - 0.75).abs() < 1e-6);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn replay_session_rejects_path_traversal() {
        let root = scratch_root("traversal");
        // Forward slash — caught by the separator check.
        let err = replay_session_in(&root, "foo/bar").expect_err("must reject /");
        assert!(format!("{err:#}").contains("path separator"));
        let err = replay_session_in(&root, "foo\\bar").expect_err("must reject \\");
        assert!(format!("{err:#}").contains("path separator"));
        // Leading dot — caught by the dot check (matches `..` + dotfiles).
        let err = replay_session_in(&root, "..").expect_err("must reject leading dot");
        assert!(format!("{err:#}").contains("starts with '.'"));
        let err = replay_session_in(&root, "../etc").expect_err("must reject ..");
        assert!(format!("{err:#}").contains("starts with '.'"));
        let err = replay_session_in(&root, "").expect_err("must reject empty");
        assert!(format!("{err:#}").contains("empty"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn replay_session_unknown_id_errors() {
        let root = scratch_root("unknown");
        let err = replay_session_in(&root, "20260101T000000Z-zzzz").expect_err("not found");
        assert!(format!("{err:#}").contains("session not found"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn validate_session_id_accepts_canonical_format() {
        // Spot-check the minted id shape.
        validate_session_id("20260518T013312Z-a4f2").expect("ok");
        validate_session_id("alpha-beta_gamma").expect("ok");
    }
}
