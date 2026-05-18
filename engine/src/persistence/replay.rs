//! Read an `events.jsonl` log back into [`Event`]s and re-fold into
//! [`EngineState`].
//!
//! Used by:
//!
//! 1. **Tests** — round-trip verification: append N events, re-open,
//!    `replay_state(&events)` must equal the in-memory fold.
//! 2. **Future restart recovery** — on engine boot, the most recent
//!    session's JSONL can be replayed to reconstruct state. Wiring
//!    that into `main.rs` is out of scope for this PR (no recovery
//!    UX yet); the primitive lands here so the next PR can compose
//!    it cleanly.
//! 3. **Post-mortem CLI** — a future `hypehouse-replay <path>` tool
//!    can stream events for offline inspection / debugging.

use crate::state::{EngineState, Event};
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Reader over a JSONL event log.
///
/// Holds a buffered reader over the file; line iteration is lazy so
/// huge logs don't have to live in memory all at once. For v0.1 the
/// only consumer is [`EventReplay::collect`] which materialises the
/// whole thing (fine for sessions of a few thousand events), but the
/// streaming iterator is the primitive future tools can build on.
pub struct EventReplay {
    reader: BufReader<File>,
}

impl EventReplay {
    /// Open an existing JSONL event log for streaming reads.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("opening event log for replay: {}", path.display()))?;
        Ok(Self {
            reader: BufReader::new(file),
        })
    }

    /// Eagerly read every line and parse each as an [`Event`].
    ///
    /// Lines that fail to deserialize abort with the line number in
    /// the error context — easier than silently skipping (a malformed
    /// line usually means the log is truncated mid-event and the rest
    /// of the file is suspect, so we surface that to the caller).
    pub fn collect(&mut self) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        let mut line_no = 0usize;
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self
                .reader
                .read_line(&mut buf)
                .with_context(|| format!("reading line {line_no} of event log"))?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let trimmed = buf.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                continue;
            }
            let ev: Event = serde_json::from_str(trimmed)
                .with_context(|| format!("parsing event on line {line_no}"))?;
            out.push(ev);
        }
        Ok(out)
    }
}

/// Fold a slice of events through [`EngineState::apply`] starting at
/// `EngineState::default()` and return the resulting state.
///
/// This is the canonical reconstruction primitive ADR-003 promises:
/// _engine state = fold over event log_.
pub fn replay_state(events: &[Event]) -> EngineState {
    let mut s = EngineState::default();
    for e in events {
        s = s.apply(e);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeckId, EventKind, EventSource};
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        std::env::temp_dir().join(format!("hh-replay-{tag}-{pid}-{nanos}.jsonl"))
    }

    #[test]
    fn replay_state_equals_inmemory_fold() {
        let events = vec![
            Event {
                id: 1,
                ts_micros: 1,
                source: EventSource::Ui,
                kind: EventKind::SessionStart,
            },
            Event {
                id: 2,
                ts_micros: 2,
                source: EventSource::Ui,
                kind: EventKind::DeckPlay { deck: DeckId::A },
            },
            Event {
                id: 3,
                ts_micros: 3,
                source: EventSource::Ui,
                kind: EventKind::Crossfader { value: 0.75 },
            },
        ];
        let folded = replay_state(&events);
        assert!(folded.session_active);
        assert!(folded.deck_a.playing);
        assert!((folded.crossfader - 0.75).abs() < 1e-6);
    }

    #[test]
    fn open_collect_round_trips_through_jsonl_file() {
        let path = temp_path("rt");
        {
            let mut f = File::create(&path).expect("create");
            for i in 0..3u64 {
                let e = Event {
                    id: i,
                    ts_micros: i as i64,
                    source: EventSource::Ui,
                    kind: EventKind::DeckPause { deck: DeckId::B },
                };
                let line = serde_json::to_string(&e).unwrap();
                writeln!(f, "{line}").unwrap();
            }
        }
        let mut r = EventReplay::open(&path).expect("open");
        let events = r.collect().expect("collect");
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].id, 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn malformed_line_surfaces_error_with_line_number() {
        let path = temp_path("malformed");
        {
            let mut f = File::create(&path).expect("create");
            writeln!(f, "{{not valid json").unwrap();
        }
        let mut r = EventReplay::open(&path).expect("open");
        let err = r.collect().expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("line 1"), "error msg lacks line number: {msg}");
        std::fs::remove_file(&path).ok();
    }
}
