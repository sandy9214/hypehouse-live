//! Audio thread + control-thread translator (ADR-004).
//!
//! This module owns everything that crosses the realtime boundary:
//!
//! * [`command`] — `AudioCommand` + `AudioCommandKind`. `Copy + Send + Sync +
//!   'static`. No heap. Fits in a fixed-size SPSC ring slot.
//! * [`ring`]    — thin wrapper over `ringbuf` SPSC. Producer is held by the
//!   control thread; consumer is held by the audio thread. Capacity 1024 per
//!   ADR-004 §"Open implementation questions".
//! * [`translator`] — pure `event_to_commands(prev, next, ev, …)` function
//!   that diffs old vs new `EngineState` and emits the right
//!   `AudioCommand`s. Lives on the control thread so it may allocate; we
//!   still use `SmallVec` to avoid heap on the common 0..4 case.
//! * [`clock`] — `EngineClock` + the atomic shared sample counter the audio
//!   thread bumps every buffer.
//! * [`io`]    — `cpal` initialization, stream callback, mixing state.
//!
//! ADR-004 hard rules (NO alloc / NO Mutex / NO blocking primitives on the
//! audio thread) are enforced by:
//!
//! * Using `ringbuf::HeapRb` allocated at construction only.
//! * `assert_no_alloc` crate gates the audio thread's hot path in tests.
//! * Clippy-driven `-D warnings`.

pub mod clock;
pub mod command;
pub mod decode;
pub mod effects;
pub mod http_source;
pub mod io;
pub mod limiter;
pub mod mixer;
pub mod perf;
pub mod pitch_tempo;
pub mod ring;
pub mod translator;
pub mod wsola;

pub use clock::{ClockSource, EngineClock, SharedClock};
pub use command::{AudioCommand, AudioCommandKind, RAMP_BUFFER_MAX};
pub use decode::{
    DecodeError, DecodeHandle, DecodeService, MidStreamFailure, MidStreamFailureKind,
    StubDecodeService, SymphoniaDecodeService, DECODER_THREAD_PANIC_CATEGORY, MAX_DECODE_SLOTS,
    MEM_PREFIX, MID_STREAM_CATEGORY, MID_STREAM_FAILURE_CAPACITY, RING_SAMPLES_500MS,
};
pub use effects::{
    descriptors as effect_descriptors, resolve_param as resolve_effect_param, Effect, EffectId,
    EffectParams, FxBank, ParamDescriptor, EFFECT_ECHO, EFFECT_FILTER, EFFECT_GATE, EFFECT_NONE,
    EFFECT_REVERB, MAX_PARAMS as EFFECT_MAX_PARAMS,
};
pub use limiter::{
    clamp_threshold_db as clamp_master_limiter_threshold_db, decode_gain_reduction_db,
    MasterLimiter, DEFAULT_THRESHOLD_DB as MASTER_LIMITER_DEFAULT_THRESHOLD_DB,
    GAIN_REDUCTION_SCALE as MASTER_LIMITER_GAIN_REDUCTION_SCALE,
    MAX_THRESHOLD_DB as MASTER_LIMITER_MAX_THRESHOLD_DB,
    MIN_THRESHOLD_DB as MASTER_LIMITER_MIN_THRESHOLD_DB,
};
pub use mixer::AudioMixer;
pub use perf::{PerfMetrics, PerfSnapshot};
pub use pitch_tempo::{
    clamp_pitch_semitones, clamp_tempo_ratio, semitones_to_ratio, PitchTempo, MAX_PITCH_SEMITONES,
    MAX_TEMPO_RATIO, MIN_PITCH_SEMITONES, MIN_TEMPO_RATIO,
};
pub use ring::{AudioConsumer, AudioProducer, AudioRing, RING_CAPACITY};
pub use translator::{
    event_to_commands, event_to_commands_with_errors, AudioCmdBatch, DecodeFailure, BAR_BEATS,
    DEFAULT_RAMP_MS,
};
pub use wsola::{
    Wsola, DEFAULT_HOP_OUT as WSOLA_DEFAULT_HOP_OUT,
    DEFAULT_SEARCH_RANGE as WSOLA_DEFAULT_SEARCH_RANGE,
    DEFAULT_WINDOW_SIZE as WSOLA_DEFAULT_WINDOW_SIZE,
};
