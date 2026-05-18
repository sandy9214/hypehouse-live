//! MIDI listener: opens an input port via `midir`, parses raw bytes against
//! a `Mapping`, and emits `Event`s to the engine's control thread over a
//! `tokio::sync::mpsc` channel.
//!
//! Threading model:
//! * `midir` invokes its callback on a vendor-provided MIDI thread (CoreMIDI
//!   on macOS, ALSA on Linux, WinMM on Windows). The callback runs in
//!   real-time priority context — must NEVER block, allocate heavily, or
//!   call into async. We do minimal byte parsing + a non-blocking
//!   `mpsc::Sender::try_send` and return.
//! * The control thread holds the matching `Receiver` and folds events into
//!   `EngineState` per ADR-003. The audio thread reads a lock-free snapshot
//!   per ADR-004.
//!
//! Lifecycle:
//! * `MidiListener::start(mapping, tx)` opens the port + spawns the callback,
//!   returning a `MidiListenerHandle` that owns the `MidiInputConnection`.
//!   Dropping the handle closes the port cleanly (midir guarantees join).
//! * No background tokio task is started — the producer side is synchronous
//!   inside the midir callback.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use midir::{MidiInput, MidiInputConnection};
use tokio::sync::mpsc;

use crate::state::{DeckId, EqBand, Event, EventKind, EventSource};

use super::clamp;
use super::mapping::{CcAction, CcBinding, Mapping, NoteAction, NoteBinding};

#[derive(Debug, thiserror::Error)]
pub enum ListenerError {
    #[error("midir init failed: {0}")]
    Init(#[from] midir::InitError),
    #[error("midir connect failed: {0}")]
    Connect(String),
    #[error("midir port info unavailable: {0}")]
    PortInfo(#[from] midir::PortInfoError),
    #[error("no MIDI input ports available")]
    NoPorts,
    #[error("no MIDI input port matched {0:?}")]
    NoMatch(String),
}

/// Handle returned by `MidiListener::start`. Drop = port closed.
pub struct MidiListenerHandle {
    /// midir keeps the OS handle alive while this is held.
    _conn: MidiInputConnection<ListenerCtx>,
    pub port_name: String,
    pub mapping_name: String,
}

/// Per-callback context: mapping snapshot, event-id counter, sender.
struct ListenerCtx {
    mapping: Arc<Mapping>,
    tx: mpsc::Sender<Event>,
    next_id: Arc<AtomicU64>,
    device: String,
}

pub struct MidiListener;

impl MidiListener {
    /// Open a MIDI input port and start dispatching events.
    ///
    /// Port selection:
    /// 1. Iterate `midir` input ports.
    /// 2. Pick the first port whose name contains `mapping.device_name_match`.
    /// 3. If `device_name_match` is empty, pick the first port available.
    /// 4. If no ports exist → `NoPorts`. If a substring is set and no port
    ///    matches → `NoMatch(substring)`.
    pub fn start(
        mapping: Mapping,
        tx: mpsc::Sender<Event>,
    ) -> Result<MidiListenerHandle, ListenerError> {
        let midi_in = MidiInput::new("hypehouse-engine-midi-in")?;
        let ports = midi_in.ports();
        if ports.is_empty() {
            return Err(ListenerError::NoPorts);
        }

        // Find a matching port.
        let needle = mapping.device_name_match.trim().to_string();
        let mut chosen: Option<(midir::MidiInputPort, String)> = None;
        for p in ports {
            let name = midi_in.port_name(&p)?;
            if needle.is_empty() || name.contains(&needle) {
                chosen = Some((p, name));
                break;
            }
        }
        let (port, port_name) = chosen.ok_or_else(|| ListenerError::NoMatch(needle.clone()))?;

        Self::start_on_port(midi_in, port, port_name, mapping, tx)
    }

    /// Lower-level variant: caller has already chosen a port (e.g. in tests
    /// using `midir::os::unix::VirtualInput` or for an explicit port pick UI).
    pub fn start_on_port(
        midi_in: MidiInput,
        port: midir::MidiInputPort,
        port_name: String,
        mapping: Mapping,
        tx: mpsc::Sender<Event>,
    ) -> Result<MidiListenerHandle, ListenerError> {
        let mapping_name = mapping.name.clone();
        let ctx = ListenerCtx {
            mapping: Arc::new(mapping),
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            device: port_name.clone(),
        };

        let conn = midi_in
            .connect(
                &port,
                "hypehouse-midi-in",
                |_ts, bytes, ctx: &mut ListenerCtx| {
                    handle_midi_message(bytes, ctx);
                },
                ctx,
            )
            .map_err(|e| ListenerError::Connect(format!("{e}")))?;

        Ok(MidiListenerHandle {
            _conn: conn,
            port_name,
            mapping_name,
        })
    }
}

/// Parse one MIDI message and emit zero-or-one `Event`s. Pure aside from the
/// `try_send` at the end; safe to call from the midir callback thread.
fn handle_midi_message(bytes: &[u8], ctx: &mut ListenerCtx) {
    if bytes.is_empty() {
        return;
    }
    let status = bytes[0];
    let kind = match status & 0xF0 {
        0x90 => {
            // Note-On. Per MIDI spec, velocity 0 = Note-Off.
            if bytes.len() < 3 {
                return;
            }
            let data1 = bytes[1];
            let velocity = clamp::clamp_midi_byte(bytes[2]);
            if velocity == 0 {
                // Treat as note-off: ignore so we only fire actions on press.
                return;
            }
            translate_note(status, data1, &ctx.mapping)
        }
        0x80 => {
            // Note-Off — current actions are press-only; ignore.
            None
        }
        0xB0 => {
            if bytes.len() < 3 {
                return;
            }
            translate_cc(status, bytes[1], bytes[2], &ctx.mapping)
        }
        0xE0 => {
            if bytes.len() < 3 {
                return;
            }
            translate_pitch_bend(status, bytes[1], bytes[2], &ctx.mapping)
        }
        _ => None, // SysEx, MTC, channel pressure, etc — not wired.
    };

    let Some(kind) = kind else { return };

    let id = ctx.next_id.fetch_add(1, Ordering::Relaxed);
    let ts_micros = chrono_micros();
    let event = Event {
        id,
        ts_micros,
        source: EventSource::Midi {
            device: ctx.device.clone(),
            mapping: "ddj200-or-user".to_string(),
        },
        kind,
    };

    // Non-blocking: if the receiver is gone or full, we drop the event.
    // Live performance: dropping > blocking the MIDI thread.
    let _ = ctx.tx.try_send(event);
}

/// Microsecond timestamp from the system clock. Used purely for event ordering
/// / replay-debugging — the audio thread relies on engine frame counters.
fn chrono_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Pure translator: Note-On → `EventKind`. Public-in-crate so tests can call
/// it without spinning up a real MIDI device.
pub(crate) fn translate_note(status: u8, data1: u8, mapping: &Mapping) -> Option<EventKind> {
    let b: &NoteBinding = mapping.note_binding(status, data1)?;
    let deck = b.deck.map(DeckId::from);
    Some(match b.action {
        NoteAction::PlayPause => EventKind::DeckPlay { deck: deck? },
        NoteAction::Cue => EventKind::DeckCue {
            deck: deck?,
            position_ms: 0,
        },
        NoteAction::HotCue => EventKind::HotCueTrigger {
            deck: deck?,
            slot: clamp::clamp_hot_cue_slot(b.slot?),
        },
        NoteAction::LoopIn => EventKind::LoopIn { deck: deck? },
        NoteAction::LoopOut => EventKind::LoopOut { deck: deck? },
        NoteAction::LoopExit => EventKind::LoopExit { deck: deck? },
        NoteAction::CopilotToggle => EventKind::CopilotEngage { deck: deck? },
        NoteAction::TakeOver => EventKind::TakeOver {
            deck: deck?,
            // Reducer-applied window; the audio-thread-aware control thread
            // stamps the actual frame count. Zero here = "compute it".
            handoff_until_frame: 0,
        },
    })
}

/// Pure translator: CC → `EventKind`.
pub(crate) fn translate_cc(
    status: u8,
    data1: u8,
    data2: u8,
    mapping: &Mapping,
) -> Option<EventKind> {
    let b: &CcBinding = mapping.cc_binding(status, data1)?;
    let deck = b.deck.map(DeckId::from);
    let [lo, hi] = b
        .range_db
        .unwrap_or([clamp::DEFAULT_EQ_DB_LO, clamp::DEFAULT_EQ_DB_HI]);

    Some(match b.action {
        CcAction::EqLow => EventKind::EqAdjust {
            deck: deck?,
            band: EqBand::Low,
            value_db: clamp::cc_to_range(data2, lo, hi),
        },
        CcAction::EqMid => EventKind::EqAdjust {
            deck: deck?,
            band: EqBand::Mid,
            value_db: clamp::cc_to_range(data2, lo, hi),
        },
        CcAction::EqHigh => EventKind::EqAdjust {
            deck: deck?,
            band: EqBand::High,
            value_db: clamp::cc_to_range(data2, lo, hi),
        },
        CcAction::Crossfader => EventKind::Crossfader {
            value: clamp::cc_to_unit(data2),
        },
        CcAction::PitchBend => {
            // CC-form pitch bend: single 7-bit value, centered at 64.
            // Map 0..=127 → -range..=+range with explicit center.
            let centered = (clamp::clamp_midi_byte(data2) as i32) - 64;
            let normalized = if centered >= 0 {
                centered as f32 / 63.0
            } else {
                centered as f32 / 64.0
            };
            let semitones = (normalized * 2.0).clamp(-12.0, 12.0);
            EventKind::PitchBend {
                deck: deck?,
                semitones,
            }
        }
    })
}

/// Pure translator: Pitch-Bend → `EventKind::PitchBend`.
pub(crate) fn translate_pitch_bend(
    status: u8,
    data1_lsb: u8,
    data2_msb: u8,
    mapping: &Mapping,
) -> Option<EventKind> {
    let b = mapping.pitch_bend_binding(status)?;
    let semitones = clamp::pitch_bend_14_to_semitones(data1_lsb, data2_msb, b.range_semitones);
    Some(EventKind::PitchBend {
        deck: DeckId::from(b.deck),
        semitones,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping() -> Mapping {
        Mapping::ddj200_default()
    }

    #[test]
    fn ddj200_play_button_emits_deck_play_a() {
        // DDJ-200 deck A play = Note-On chan 0, note 0x0B, vel 127.
        let kind = translate_note(0x90, 0x0B, &mapping()).expect("known mapping");
        match kind {
            EventKind::DeckPlay { deck } => assert_eq!(deck, DeckId::A),
            other => panic!("expected DeckPlay, got {other:?}"),
        }
    }

    #[test]
    fn ddj200_play_button_deck_b_emits_deck_play_b() {
        // Deck B play = channel 1, note 0x0B → status 0x91.
        let kind = translate_note(0x91, 0x0B, &mapping()).expect("known mapping");
        match kind {
            EventKind::DeckPlay { deck } => assert_eq!(deck, DeckId::B),
            other => panic!("expected DeckPlay, got {other:?}"),
        }
    }

    #[test]
    fn ddj200_crossfader_64_emits_value_half() {
        // Crossfader CC = 0xBF (channel 15 / master), note 0x1F per DDJ-200 map.
        let kind = translate_cc(0xBF, 0x1F, 64, &mapping()).expect("crossfader binding");
        match kind {
            EventKind::Crossfader { value } => {
                // 64/127 ≈ 0.5039 — close enough to half.
                assert!((value - 0.5).abs() < 0.01, "got {value}");
            }
            other => panic!("expected Crossfader, got {other:?}"),
        }
    }

    #[test]
    fn ddj200_crossfader_endpoints() {
        let m = mapping();
        let lo = translate_cc(0xBF, 0x1F, 0, &m).expect("xfader");
        match lo {
            EventKind::Crossfader { value } => assert_eq!(value, 0.0),
            other => panic!("got {other:?}"),
        }
        let hi = translate_cc(0xBF, 0x1F, 127, &m).expect("xfader");
        match hi {
            EventKind::Crossfader { value } => assert!((value - 1.0).abs() < 1e-6),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unknown_cc_returns_none() {
        let m = mapping();
        assert!(translate_cc(0xB0, 0x7F, 64, &m).is_none());
        assert!(translate_cc(0xBF, 0x7E, 1, &m).is_none());
    }

    #[test]
    fn unknown_note_returns_none() {
        let m = mapping();
        assert!(translate_note(0x90, 0x7F, &m).is_none());
        assert!(translate_note(0x9F, 0x00, &m).is_none());
    }

    #[test]
    fn pitch_bend_center_emits_zero_semitones() {
        // ±2 semitone range per DDJ-200 default.
        let m = mapping();
        let kind = translate_pitch_bend(0xE0, 0x00, 0x40, &m).expect("pb binding");
        match kind {
            EventKind::PitchBend { deck, semitones } => {
                assert_eq!(deck, DeckId::A);
                assert!(semitones.abs() < 1e-6, "got {semitones}");
            }
            other => panic!("expected PitchBend, got {other:?}"),
        }
    }

    #[test]
    fn pitch_bend_full_positive_is_plus_two_semitones() {
        let m = mapping();
        let kind = translate_pitch_bend(0xE0, 0x7F, 0x7F, &m).expect("pb binding");
        match kind {
            EventKind::PitchBend { semitones, .. } => {
                assert!((semitones - 2.0).abs() < 1e-3, "got {semitones}");
            }
            other => panic!("expected PitchBend, got {other:?}"),
        }
    }

    #[test]
    fn pitch_bend_full_negative_is_minus_two_semitones() {
        let m = mapping();
        let kind = translate_pitch_bend(0xE0, 0x00, 0x00, &m).expect("pb binding");
        match kind {
            EventKind::PitchBend { semitones, .. } => {
                assert!((semitones + 2.0).abs() < 1e-3, "got {semitones}");
            }
            other => panic!("expected PitchBend, got {other:?}"),
        }
    }

    #[test]
    fn hostile_midi_byte_does_not_panic() {
        let m = mapping();
        // High bit set on data bytes (illegal but seen in fuzz)
        let _ = translate_cc(0xBF, 0x1F, 0xFF, &m);
        let _ = translate_pitch_bend(0xE0, 0xFF, 0xFF, &m);
        let _ = translate_note(0x90, 0xFF, &m);
        // No panic = pass.
    }

    #[test]
    fn handle_midi_message_short_message_safe() {
        // Construct a real ListenerCtx without spinning up midir.
        let (tx, _rx) = mpsc::channel(16);
        let mut ctx = ListenerCtx {
            mapping: Arc::new(mapping()),
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            device: "test".into(),
        };
        // Truncated CC message: must not panic.
        handle_midi_message(&[0xBF, 0x1F], &mut ctx);
        handle_midi_message(&[0x90], &mut ctx);
        handle_midi_message(&[], &mut ctx);
        handle_midi_message(&[0xE0, 0x00], &mut ctx);
    }

    #[test]
    fn handle_midi_message_emits_event_for_play() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut ctx = ListenerCtx {
            mapping: Arc::new(mapping()),
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            device: "test".into(),
        };
        // DDJ-200 deck A play press: 0x90 0x0B 0x7F
        handle_midi_message(&[0x90, 0x0B, 0x7F], &mut ctx);
        let ev = rx.try_recv().expect("event emitted");
        match ev.kind {
            EventKind::DeckPlay { deck } => assert_eq!(deck, DeckId::A),
            other => panic!("expected DeckPlay, got {other:?}"),
        }
        match ev.source {
            EventSource::Midi { ref device, .. } => assert_eq!(device, "test"),
            ref other => panic!("expected Midi source, got {other:?}"),
        }
    }

    #[test]
    fn handle_midi_message_note_on_velocity_zero_is_silent() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut ctx = ListenerCtx {
            mapping: Arc::new(mapping()),
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            device: "test".into(),
        };
        // Note-on with velocity 0 == note-off; suppress.
        handle_midi_message(&[0x90, 0x0B, 0x00], &mut ctx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn handle_midi_message_unknown_cc_emits_nothing() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut ctx = ListenerCtx {
            mapping: Arc::new(mapping()),
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            device: "test".into(),
        };
        handle_midi_message(&[0xB0, 0x7E, 64], &mut ctx);
        assert!(rx.try_recv().is_err());
    }

    /// CC-form pitch bend (some user mappings prefer it over real PB):
    /// verified through the Mapping's CC action path.
    #[test]
    fn cc_form_pitch_bend_center_is_zero() {
        let json = r#"{
            "device_name_match": "x",
            "ccs": { "0xB0:0x40": { "deck": "A", "action": "pitch_bend" } }
        }"#;
        let m = Mapping::from_json_str(json).unwrap();
        let kind = translate_cc(0xB0, 0x40, 64, &m).expect("binding");
        match kind {
            EventKind::PitchBend { semitones, .. } => {
                assert!(semitones.abs() < 1e-6, "got {semitones}");
            }
            other => panic!("got {other:?}"),
        }
    }

    /// User-defined EQ range overrides the default.
    #[test]
    fn cc_eq_uses_mapping_range() {
        let json = r#"{
            "device_name_match": "x",
            "ccs": { "0xB0:0x10": { "deck": "A", "action": "eq_low", "range_db": [-10, 10] } }
        }"#;
        let m = Mapping::from_json_str(json).unwrap();
        let kind = translate_cc(0xB0, 0x10, 127, &m).expect("binding");
        match kind {
            EventKind::EqAdjust { value_db, .. } => assert!((value_db - 10.0).abs() < 1e-3),
            other => panic!("got {other:?}"),
        }
    }
}
