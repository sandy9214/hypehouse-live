//! hypehouse-engine — library surface.
//!
//! Module layout (filled in as v0.1 progresses):
//!
//! * `state`    — event-sourced state log + reducer (ADR-003).
//! * `deck`     — single-deck state machine (play/cue/pitch/EQ/loop/hot cues).
//! * `mixer`    — crossfader + master + recording.
//! * `audio_io` — cpal callback, sample-accurate scheduling.
//! * `midi`     — midir listener + Pioneer DDJ-200 default mapping (ADR-004).
//! * `bridge`   — WebSocket to UI + JSON-RPC to copilot.

pub mod state;

pub use state::{Deck, DeckId, EngineState, Event, EventKind, EventSource};
