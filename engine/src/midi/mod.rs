//! MIDI subsystem (ADR-001/004).
//!
//! Responsibilities:
//! 1. Open the user's MIDI input device via `midir`.
//! 2. Parse incoming raw MIDI bytes (Note On/Off, CC, Pitch Bend).
//! 3. Look up the mapping → translate to engine `Event`s.
//! 4. Defensively clamp every value before emitting.
//! 5. Emit to a `tokio::sync::mpsc` channel handed to the control thread.
//!
//! Module layout:
//! * `mapping`  — JSON-deserializable schema + DDJ-200 default (embedded).
//! * `clamp`    — value-clamping primitives. Every MIDI-derived value
//!   passes through here before becoming an `Event`.
//! * `listener` — `MidiListener::start` opens a port + spawns the callback.
//!
//! The default mapping ships at `mappings/ddj200.json` and is bundled into
//! the binary via `include_str!`. Users can override by setting the
//! `HYPEHOUSE_MIDI_MAPPING` environment variable to a JSON file path.

pub mod clamp;
pub mod listener;
pub mod mapping;

pub use listener::{ListenerError, MidiListener, MidiListenerHandle};
pub use mapping::{
    CcAction, CcBinding, MapDeck, Mapping, MappingError, NoteAction, NoteBinding, PitchBendBinding,
};
