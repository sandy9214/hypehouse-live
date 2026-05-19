//! hypehouse-engine — library surface.
//!
//! Module layout (filled in as v0.1 progresses):
//!
//! * `state`      — event-sourced state log + reducer (ADR-003).
//! * `audio`      — control→audio translator + cpal callback + ring (ADR-004).
//! * `deck`       — single-deck state machine (play/cue/pitch/EQ/loop/hot cues).
//! * `mixer`      — crossfader + master + recording.
//! * `midi`       — midir listener + Pioneer DDJ-200 default mapping (ADR-004).
//! * `clock_sync` — peer-to-peer clock sync (Ableton Link scaffold, ADR-007 §v0.2).
//! * `bridge`     — WebSocket to UI + JSON-RPC to copilot.

pub mod audio;
pub mod bridge;
pub mod clock_sync;
pub mod midi;
pub mod oneshot_sweeper;
pub mod persistence;
pub mod recording;
pub mod state;
pub mod telemetry;

pub use state::{Deck, DeckId, EngineState, Event, EventKind, EventSource};
