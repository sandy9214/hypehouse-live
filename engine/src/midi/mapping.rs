//! MIDI → engine event mapping (ADR-001/004 stub).
//!
//! Schema v0 (JSON):
//! ```json
//! {
//!   "device_name_match": "DDJ-200",
//!   "notes": {
//!     "0x90:0x0B": { "deck": "A", "action": "play_pause" }
//!   },
//!   "ccs": {
//!     "0xB0:0x1F": { "deck": "A", "action": "eq_low", "range_db": [-26, 6] },
//!     "0xB0:0x20": { "action": "crossfader" }
//!   },
//!   "pitch_bends": {
//!     "0xE0": { "deck": "A", "range_semitones": 2.0 }
//!   }
//! }
//! ```
//!
//! Status byte includes channel (low nibble); we key off the full byte so the
//! mapping can target a specific channel. DDJ-200 ships on channel 0.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::state::DeckId;

/// MIDI status:data1 lookup key. e.g. `"0x90:0x0B"` for Note-On channel 0, note 11.
pub type StatusKey = String;

/// MIDI status-only key (for pitch bend which has no fixed data1). e.g. `"0xE0"`.
pub type StatusOnlyKey = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mapping {
    /// Substring match against `MidiInput::ports()` port-name strings. The
    /// listener selects the first port whose name contains this substring.
    /// Empty string = first port available.
    #[serde(default)]
    pub device_name_match: String,

    /// Human-readable name of this mapping, recorded in `EventSource::Midi`.
    #[serde(default = "default_mapping_name")]
    pub name: String,

    /// Note-On / Note-Off → action.
    #[serde(default)]
    pub notes: HashMap<StatusKey, NoteBinding>,

    /// Control-Change → action.
    #[serde(default)]
    pub ccs: HashMap<StatusKey, CcBinding>,

    /// Pitch-Bend → action. Keyed by status byte only (e.g. `"0xE0"`) because
    /// pitch-bend uses 14-bit data spread across data1+data2.
    #[serde(default)]
    pub pitch_bends: HashMap<StatusOnlyKey, PitchBendBinding>,
}

fn default_mapping_name() -> String {
    "user-mapping".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteBinding {
    #[serde(default)]
    pub deck: Option<MapDeck>,
    pub action: NoteAction,
    /// Optional slot for hot cues (0..=7).
    #[serde(default)]
    pub slot: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CcBinding {
    #[serde(default)]
    pub deck: Option<MapDeck>,
    pub action: CcAction,
    /// EQ-specific dB range. Defaults to `[-26.0, 6.0]` (industry standard).
    #[serde(default)]
    pub range_db: Option<[f32; 2]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitchBendBinding {
    pub deck: MapDeck,
    /// Semitones at full deflection (±). Default ±2 semitones (typical DJ jog/pitch range).
    #[serde(default = "default_pitch_range")]
    pub range_semitones: f32,
}

fn default_pitch_range() -> f32 {
    2.0
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MapDeck {
    A,
    B,
}

impl From<MapDeck> for DeckId {
    fn from(m: MapDeck) -> Self {
        match m {
            MapDeck::A => DeckId::A,
            MapDeck::B => DeckId::B,
        }
    }
}

/// Actions a Note message can trigger.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoteAction {
    PlayPause,
    Cue,
    HotCue,
    LoopIn,
    LoopOut,
    LoopExit,
    CopilotToggle,
    TakeOver,
}

/// Actions a CC message can trigger. CC values are continuous (0..=127).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CcAction {
    EqLow,
    EqMid,
    EqHigh,
    Crossfader,
    /// CC-form pitch bend (some controllers use CC for jog/pitch even though
    /// MIDI spec has a dedicated pitch-bend message).
    PitchBend,
}

#[derive(Debug, thiserror::Error)]
pub enum MappingError {
    #[error("failed to read mapping file {path:?}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid mapping JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("mapping validation failed: {0}")]
    Invalid(String),
}

impl Mapping {
    /// Default built-in Pioneer DDJ-200 mapping. Embedded via `include_str!`
    /// so the engine works zero-config out of the box.
    pub fn ddj200_default() -> Self {
        Self::from_json_str(include_str!("mappings/ddj200.json"))
            .expect("embedded ddj200.json is invalid — build-time bug")
    }

    /// Resolve a mapping per env var policy:
    /// `HYPEHOUSE_MIDI_MAPPING` set → load that file; otherwise the embedded
    /// DDJ-200 default.
    pub fn resolve_from_env() -> Result<Self, MappingError> {
        match std::env::var("HYPEHOUSE_MIDI_MAPPING") {
            Ok(path) if !path.trim().is_empty() => Self::from_path(&path),
            _ => Ok(Self::ddj200_default()),
        }
    }

    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self, MappingError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| MappingError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json_str(&raw)
    }

    pub fn from_json_str(s: &str) -> Result<Self, MappingError> {
        let m: Self = serde_json::from_str(s)?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), MappingError> {
        for (k, b) in &self.notes {
            parse_status_data_key(k).map_err(MappingError::Invalid)?;
            if matches!(b.action, NoteAction::HotCue) && b.slot.is_none() {
                return Err(MappingError::Invalid(format!(
                    "hot_cue binding {k} requires `slot` (0..=7)"
                )));
            }
            if let Some(slot) = b.slot {
                if slot > 7 {
                    return Err(MappingError::Invalid(format!(
                        "hot_cue binding {k} slot must be 0..=7, got {slot}"
                    )));
                }
            }
            if b.deck.is_none() && needs_deck_note(b.action) {
                return Err(MappingError::Invalid(format!(
                    "note binding {k} action {:?} requires `deck`",
                    b.action
                )));
            }
        }
        for (k, b) in &self.ccs {
            parse_status_data_key(k).map_err(MappingError::Invalid)?;
            if let Some([lo, hi]) = b.range_db {
                if !lo.is_finite() || !hi.is_finite() || hi <= lo {
                    return Err(MappingError::Invalid(format!(
                        "cc binding {k} range_db invalid: [{lo}, {hi}]"
                    )));
                }
            }
            if b.deck.is_none() && needs_deck_cc(b.action) {
                return Err(MappingError::Invalid(format!(
                    "cc binding {k} action {:?} requires `deck`",
                    b.action
                )));
            }
        }
        for (k, _) in &self.pitch_bends {
            parse_status_only_key(k).map_err(MappingError::Invalid)?;
        }
        Ok(())
    }

    /// Look up a note (status byte = 0x80-0x9F) binding.
    pub fn note_binding(&self, status: u8, data1: u8) -> Option<&NoteBinding> {
        let key = format_status_data_key(status, data1);
        self.notes.get(&key)
    }

    /// Look up a CC binding (status byte = 0xB0-0xBF).
    pub fn cc_binding(&self, status: u8, data1: u8) -> Option<&CcBinding> {
        let key = format_status_data_key(status, data1);
        self.ccs.get(&key)
    }

    /// Look up a pitch-bend binding (status byte = 0xE0-0xEF).
    pub fn pitch_bend_binding(&self, status: u8) -> Option<&PitchBendBinding> {
        let key = format_status_only_key(status);
        self.pitch_bends.get(&key)
    }
}

fn needs_deck_note(_a: NoteAction) -> bool {
    // All current note actions are deck-scoped. Kept as a function so future
    // session-global actions (e.g. SessionStart) can opt-out cleanly.
    true
}

fn needs_deck_cc(a: CcAction) -> bool {
    !matches!(a, CcAction::Crossfader)
}

fn format_status_data_key(status: u8, data1: u8) -> String {
    format!("0x{status:02X}:0x{data1:02X}")
}

fn format_status_only_key(status: u8) -> String {
    format!("0x{status:02X}")
}

fn parse_status_data_key(k: &str) -> Result<(u8, u8), String> {
    let parts: Vec<&str> = k.split(':').collect();
    if parts.len() != 2 {
        return Err(format!("malformed key {k:?} — expected `0xSS:0xDD`"));
    }
    let s = parse_hex_byte(parts[0]).map_err(|e| format!("key {k:?} status: {e}"))?;
    let d = parse_hex_byte(parts[1]).map_err(|e| format!("key {k:?} data1: {e}"))?;
    Ok((s, d))
}

fn parse_status_only_key(k: &str) -> Result<u8, String> {
    parse_hex_byte(k).map_err(|e| format!("key {k:?}: {e}"))
}

fn parse_hex_byte(s: &str) -> Result<u8, String> {
    let s = s.trim();
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| format!("expected 0x-prefixed hex byte, got {s:?}"))?;
    u8::from_str_radix(stripped, 16).map_err(|e| format!("invalid hex byte {s:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddj200_default_parses() {
        let m = Mapping::ddj200_default();
        assert!(m.device_name_match.contains("DDJ-200"));
        assert!(!m.notes.is_empty());
        assert!(!m.ccs.is_empty());
    }

    #[test]
    fn invalid_json_returns_helpful_err() {
        let err = Mapping::from_json_str("{ not valid json ").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid mapping JSON"), "got: {msg}");
    }

    #[test]
    fn malformed_key_returns_invalid() {
        let json = r#"{
            "device_name_match": "test",
            "notes": { "noprefix": { "deck": "A", "action": "play_pause" } }
        }"#;
        let err = Mapping::from_json_str(json).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("malformed key") || msg.contains("expected 0x"), "got: {msg}");
    }

    #[test]
    fn hot_cue_without_slot_rejected() {
        let json = r#"{
            "device_name_match": "test",
            "notes": { "0x90:0x01": { "deck": "A", "action": "hot_cue" } }
        }"#;
        let err = Mapping::from_json_str(json).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("slot"), "got: {msg}");
    }

    #[test]
    fn hot_cue_slot_out_of_range_rejected() {
        let json = r#"{
            "device_name_match": "test",
            "notes": { "0x90:0x01": { "deck": "A", "action": "hot_cue", "slot": 9 } }
        }"#;
        let err = Mapping::from_json_str(json).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("slot"), "got: {msg}");
    }

    #[test]
    fn cc_range_db_inverted_rejected() {
        let json = r#"{
            "ccs": { "0xB0:0x10": { "deck": "A", "action": "eq_low", "range_db": [6, -26] } }
        }"#;
        let err = Mapping::from_json_str(json).unwrap_err();
        assert!(format!("{err}").contains("range_db"));
    }

    #[test]
    fn lookup_returns_binding() {
        let m = Mapping::ddj200_default();
        // DDJ-200 deck A play button — 0x90:0x0B per Pioneer mapping doc.
        let b = m.note_binding(0x90, 0x0B).expect("play binding present");
        assert_eq!(b.action, NoteAction::PlayPause);
        assert_eq!(b.deck, Some(MapDeck::A));
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let m = Mapping::ddj200_default();
        assert!(m.note_binding(0x9F, 0xFF).is_none());
        assert!(m.cc_binding(0xBF, 0xFF).is_none());
    }
}
