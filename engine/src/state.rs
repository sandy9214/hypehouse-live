//! Event-sourced engine state (ADR-003).
//!
//! `EngineState` is the fold of an event log. Every UI/MIDI/copilot input
//! becomes an `Event`; the reducer applies it deterministically. No shared
//! mutable state across threads — the audio thread reads a lock-free
//! snapshot of `EngineState` and renders.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Inline capacity for the per-deck downbeat grid. A 4/4 track at 120 BPM
/// in 4-bar phrases generates ~1 downbeat every 8 beats; a 5-minute set
/// (300s) at 120 BPM has ~37 downbeats. We round up to 64 so most pop /
/// EDM tracks fit on the stack — heap spill is only paid by edge-case
/// long tracks (≥10 min at fast tempo). Tracks with more downbeats are
/// truncated by the reducer; see `EventKind::DeckLoad` handling.
pub const DOWNBEATS_INLINE_CAPACITY: usize = 64;

/// Per-deck downbeat grid (millisecond positions inside the track). u32
/// ceiling = ~71 minutes, more than any sane DJ track. Storing u32 keeps
/// the SmallVec small (256B inline vs 512B for u64) which matters because
/// `Deck` is cloned by the pure reducer on every event.
pub type DownbeatGrid = SmallVec<[u32; DOWNBEATS_INLINE_CAPACITY]>;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckId {
    A,
    B,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EqBand {
    Low,
    Mid,
    High,
}

/// Crossfader response curve (pro-DJ convention).
///
/// Mirrors the four curves that hardware DJ mixers expose on a back-panel
/// switch. The variant drives the per-block gain lookup in
/// [`crate::audio::mixer::AudioMixer::render`]:
///
/// * `Linear`  — smooth `gain_a = 1-x`, `gain_b = x`. Classic long-blend
///   curve; loses ~3 dB of master energy in the centre.
/// * `Dipped`  — equal-power `gain_a = sqrt(1-x)`, `gain_b = sqrt(x)`.
///   Each side is **-3 dB** at centre, so summed power stays flat across
///   the full travel. Best for vocal-on-vocal blends.
/// * `Sharp`   — full-amplitude on the dominant side until the narrow
///   centre region, then a linear ramp. Aggressive cut for hip-hop /
///   scratch styles where you want both decks audible only inside a
///   ±0.05 window. The 0.1-wide ramp prevents the click an instant snap
///   would create.
/// * `Scratch` — almost-instant cut. Full A for x ≤ 0.05, linear in the
///   ±0.05 window around 0.5 is **not** used here — instead the curve
///   snaps on a single 0.10-wide window across the very edges. The
///   resulting curve sounds like a turntable cut-in; it's *not* a true
///   square wave (which would zip) but is sharper than `Sharp`.
///
/// Wire / serde representation: external-tag default. JSON values are
/// the variant names (`"Linear"`, `"Dipped"`, `"Sharp"`, `"Scratch"`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CrossfaderCurve {
    /// Smooth `gain_a = 1-x`, `gain_b = x` (default; existing behaviour
    /// pre-curve PR).
    #[default]
    Linear,
    /// Equal-power `gain_a = sqrt(1-x)`, `gain_b = sqrt(x)`. -3 dB
    /// dip on each side at centre.
    Dipped,
    /// Aggressive ramp inside a narrow `±0.05` window around centre;
    /// full-amplitude outside.
    Sharp,
    /// Near-instant cut: full A until `x ≥ 0.95`, full B above.
    /// Linear blend in the 0.10-wide cliff window.
    Scratch,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EventSource {
    Ui,
    Midi { device: String, mapping: String },
    Copilot,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TrackRef {
    pub id: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EventKind {
    DeckLoad {
        deck: DeckId,
        track: TrackRef,
        /// Pre-analyzed BPM + beat-grid anchor; sourced from the copilot
        /// analyzer service (HypeHouse v1 carry-over). Required for
        /// beat-matching. ADR-002 council review (Codex).
        bpm: f32,
        beat_grid_anchor_ms: u64,
        /// Downbeat (bar-start) positions in milliseconds, sourced from
        /// the copilot's madmom DBNDownBeatTracker pass. Optional so old
        /// payloads (before beat-grid analysis landed) still deserialize
        /// — they just leave the deck with an empty downbeat grid and
        /// phrase-aligned transitions fall back to bar-grid math derived
        /// from `beat_grid_anchor_ms` + `beat_period_ms × 4`.
        ///
        /// Truncated to `DOWNBEATS_INLINE_CAPACITY` (64) entries inside
        /// the reducer — see :meth:`EngineState::apply`. Most tracks fit
        /// comfortably; the cap protects the inline `SmallVec` budget.
        #[serde(default)]
        downbeats_ms: Vec<u32>,
        /// 8-slot hot-cue grid sourced from the copilot's library
        /// (added in the hot-cue persistence PR). `Some(ms)` = saved
        /// cue position relative to track start, `None` = empty slot.
        /// Optional so pre-this-PR `DeckLoad` payloads still
        /// deserialize via serde's default (all None) — they just
        /// load a track with no preset hot-cues. The shape mirrors
        /// `Deck::hot_cues` exactly so the reducer's assignment is a
        /// direct copy.
        #[serde(default = "default_hot_cues")]
        hot_cues: [Option<u64>; 8],
        /// Per-track loudness-leveler gain in **decibels**, sourced
        /// from the copilot's :func:`copilot.loudness.compute_lufs`
        /// pass. `0.0` = no change (the engine's pre-loudness-PR
        /// behaviour). Positive = boost (the track was mastered
        /// quieter than the streaming reference, e.g. a -23 LUFS
        /// jazz cut → +9 dB). Negative = cut (a +8 LUFS EDM master
        /// → -6 dB). Optional via serde default so old payloads
        /// (and tracks whose copilot row pre-dates the v7 schema)
        /// still deserialize — they just load with 0 dB which is
        /// audibly identical to the v0 mixer.
        ///
        /// The mixer applies `10^(track_gain_db / 20)` to every
        /// sample on the deck slice, post-decode + pre-effects.
        #[serde(default)]
        track_gain_db: f32,
    },
    /// ADR review Groq: explicit DeckUnload so the engine can free buffers
    /// and clear state cleanly (vs. relying on DeckLoad implicit replace).
    DeckUnload {
        deck: DeckId,
    },
    /// Stem-aware load — alternative to `DeckLoad` that points the deck
    /// at **four pre-separated stem WAVs** (vocals / drums / bass /
    /// other) produced by the copilot's `stems.py` (demucs). The
    /// engine opens each stem as an independent decode handle + mixes
    /// them per render block with the deck's `stem_gains` envelope,
    /// unlocking "kill the vocals" / "drums only" mashup tricks.
    ///
    /// `stem_paths` ordering MUST be:
    /// * `0` = vocals
    /// * `1` = drums
    /// * `2` = bass
    /// * `3` = other
    ///
    /// Mutually exclusive with `DeckLoad` on the same deck: the
    /// reducer clears any prior full-mix `loaded` TrackRef when a
    /// stem load lands, and vice versa (a later `DeckLoad` clears the
    /// stem-load state). This avoids ambiguity in the audio path —
    /// the deck is either streaming the full mix or the 4-stem split,
    /// never both.
    DeckLoadStems {
        deck: DeckId,
        track: TrackRef,
        /// 4 stem WAV paths in canonical order (vocals/drums/bass/other).
        stem_paths: [String; 4],
        /// Same beatgrid + hot-cue payload shape as `DeckLoad` so the
        /// copilot can reuse the analyzer it already ran on the full
        /// mix.
        bpm: f32,
        beat_grid_anchor_ms: u64,
        #[serde(default)]
        downbeats_ms: Vec<u32>,
        #[serde(default = "default_hot_cues")]
        hot_cues: [Option<u64>; 8],
    },
    /// Per-stem linear gain (0..1). Indexed by canonical stem order
    /// (0=vocals 1=drums 2=bass 3=other). Out-of-range indices are
    /// silently ignored by the reducer + translator.
    SetStemGain {
        deck: DeckId,
        stem: u8,
        gain: f32,
    },
    DeckPlay {
        deck: DeckId,
    },
    DeckPause {
        deck: DeckId,
    },
    DeckCue {
        deck: DeckId,
        position_ms: u64,
    },
    Crossfader {
        value: f32,
    },
    /// Select the crossfader response curve. See [`CrossfaderCurve`]
    /// for variant semantics. Pure state mutation — the audio thread
    /// receives a single `SetCrossfaderCurve` command and switches its
    /// per-block gain lookup. No audio-thread allocation.
    SetCrossfaderCurve {
        curve: CrossfaderCurve,
    },
    EqAdjust {
        deck: DeckId,
        band: EqBand,
        value_db: f32,
    },
    HotCueSet {
        deck: DeckId,
        slot: u8,
        position_ms: u64,
    },
    HotCueTrigger {
        deck: DeckId,
        slot: u8,
    },
    LoopIn {
        deck: DeckId,
    },
    LoopOut {
        deck: DeckId,
    },
    LoopExit {
        deck: DeckId,
    },
    /// Pure pitch shift in semitones (key change). **Tempo unchanged**
    /// — drives only the per-deck pitch resampler stage. Use
    /// `TempoBend` for tempo control. Clamped to ±12 by the reducer.
    PitchBend {
        deck: DeckId,
        semitones: f32,
    },
    /// Independent tempo control — ratio of playback speed. 1.0 =
    /// normal, < 1 = slower, > 1 = faster. Pitch is preserved by the
    /// per-deck `PitchTempo` cascade. Reducer clamps to
    /// `[audio::MIN_TEMPO_RATIO, audio::MAX_TEMPO_RATIO]` and rejects
    /// non-finite inputs (treats them as 1.0). Companion to
    /// `PitchBend`; the two knobs are independent in the public API
    /// (v0.1 cascade implementation has a documented limitation — see
    /// `engine/src/audio/pitch_tempo.rs`).
    TempoBend {
        deck: DeckId,
        ratio: f32,
    },
    /// Convenience event — reset both `pitch_semitones` and
    /// `tempo_ratio` on a deck to their defaults (0.0 / 1.0). Emits
    /// `AudioCommandKind::PitchTempoReset` so the audio thread also
    /// resets the rubato cascade state.
    PitchTempoReset {
        deck: DeckId,
    },
    /// Phase nudge — apply manual offset to deck's beat grid for sync (ADR-007).
    PhaseNudge {
        deck: DeckId,
        delta_ms: i32,
    },
    /// Set the session master BPM (ADR-007 §v0.1). Drives the MIDI clock
    /// OUT tick thread + any future Ableton Link master. Non-finite or
    /// non-positive values are clamped to the previous master BPM by
    /// the reducer (no-op apply).
    SetMasterBpm {
        bpm: f32,
    },
    /// Effects (ADR-006).
    EffectAssign {
        deck: DeckId,
        slot: u8,
        effect_id: u32,
    },
    EffectClear {
        deck: DeckId,
        slot: u8,
    },
    EffectParam {
        deck: DeckId,
        slot: u8,
        param: String,
        value: f32,
    },
    EffectWetDry {
        deck: DeckId,
        slot: u8,
        value: f32,
    },
    EffectEnable {
        deck: DeckId,
        slot: u8,
        enabled: bool,
    },
    /// ADR-006 — swap two slot positions in a deck's effects chain.
    /// Reorders the slot contents (effect_id + params + wet_dry +
    /// enabled) in place. Both indices are clamped to the valid
    /// `0..3` range; same-slot swap is a no-op. Used by the UI's
    /// drag-drop reordering + keyboard shift-up / shift-down.
    EffectSwapSlots {
        deck: DeckId,
        slot_a: u8,
        slot_b: u8,
    },
    CopilotEngage {
        deck: DeckId,
    },
    CopilotDisengage {
        deck: DeckId,
    },
    /// User pre-empts the AI mid-transition. ADR-005 defines a bounded
    /// 1-bar handoff envelope; the audio thread continues AI automation
    /// for that window while ramping user inputs in. The control thread
    /// computes `handoff_until_frame` from current engine clock + the
    /// deck's beat_period_ms (4 beats = 1 bar at 4/4) and stamps it on
    /// the event before applying — reducer is then pure.
    TakeOver {
        deck: DeckId,
        handoff_until_frame: u64,
    },
    /// Toggle the master-bus soft-clip limiter (default `true` for
    /// safety — both the live mix and the recorded `master.wav` are
    /// kept inside ±1.0 when both decks are hot + effects are active).
    /// See [`crate::audio::limiter`] for the algorithm.
    SetMasterLimiterEnabled {
        enabled: bool,
    },
    /// Set the master-bus limiter's threshold in dB. Reducer clamps to
    /// `[audio::MASTER_LIMITER_MIN_THRESHOLD_DB, audio::MASTER_LIMITER_MAX_THRESHOLD_DB]`
    /// (= `[-24.0, 0.0]`). Non-finite inputs fall back to the default
    /// (-0.5 dB) per `audio::clamp_master_limiter_threshold_db`.
    SetMasterLimiterThreshold {
        threshold_db: f32,
    },
    SessionStart,
    SessionEnd,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Event {
    pub id: u64,
    pub ts_micros: i64,
    pub source: EventSource,
    pub kind: EventKind,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Deck {
    pub loaded: Option<TrackRef>,
    pub playing: bool,
    pub position_ms: u64,
    /// **Pure pitch shift** in semitones — independent of tempo (post
    /// the pitch/tempo-independent PR). 0.0 = original key. Clamped to
    /// ±12. Drives stage 1 of the per-deck rubato cascade in
    /// `audio::pitch_tempo`.
    pub pitch_semitones: f32,
    /// **Tempo ratio** — playback-speed multiplier independent of
    /// pitch. 1.0 = normal speed. Clamped to
    /// `[audio::MIN_TEMPO_RATIO, audio::MAX_TEMPO_RATIO]` (default
    /// 0.5..2.0). Drives stage 2 of the per-deck rubato cascade.
    #[serde(default = "default_tempo_ratio")]
    pub tempo_ratio: f32,
    pub eq_low_db: f32,
    pub eq_mid_db: f32,
    pub eq_high_db: f32,
    pub loop_in_ms: Option<u64>,
    pub loop_out_ms: Option<u64>,
    pub loop_active: bool,
    pub hot_cues: [Option<u64>; 8],
    pub copilot_engaged: bool,
    /// Per-track loudness-leveler gain in **decibels**, set by
    /// `EventKind::DeckLoad` from the copilot's pre-computed
    /// `track_gain_db` library column. `0.0` = no change (the
    /// engine's pre-loudness-PR behaviour). The mixer's `DeckHot`
    /// mirror is what the audio thread actually multiplies into the
    /// deck slice; the state-side copy exists so a snapshot consumer
    /// (UI, persistence) can show the user how much gain is being
    /// applied per deck.
    ///
    /// Clamped by the copilot (`copilot/loudness.py` caps to
    /// `[-20, +14]` dB) so the audio-side multiply never trips the
    /// master limiter unnecessarily. The reducer additionally
    /// guards against non-finite payloads (treats them as 0.0)
    /// since serde will happily accept any f32 bit pattern.
    #[serde(default)]
    pub track_gain_db: f32,
    /// Council ADR-002 review (Codex): live mixing needs beatgrid + tempo
    /// + phase on the deck or beat-matching can't be reasoned about.
    pub bpm: f32,
    pub beat_grid_anchor_ms: u64, // ms of beat 0 in the track
    pub beat_period_ms: f32,      // milliseconds per beat (= 60_000 / bpm)
    pub phase_offset_ms: i32,     // user-applied phase nudge (±)
    /// Downbeat (bar-start) positions in ms, populated from the copilot
    /// analyzer's madmom pass on `DeckLoad`. Inline capacity =
    /// `DOWNBEATS_INLINE_CAPACITY` (64) — sufficient for most 3-5 minute
    /// tracks at common tempos. Tracks with more downbeats are truncated
    /// to the first 64 in the reducer (see `EngineState::apply`); the
    /// truncation is intentional and documented on the field rather than
    /// dropped silently — callers should not see runtime cliffs at the
    /// boundary.
    #[serde(default, skip_serializing_if = "SmallVec::is_empty")]
    pub downbeats: DownbeatGrid,
    /// Effects chain (ADR-006). 3 slots per deck.
    pub effects: [EffectSlot; 3],
    /// Co-pilot takeover handoff window end (ADR-005). 0 = no handoff active.
    pub handoff_until_frame: u64,
    /// Per-stem linear gain when the deck is loaded with separated
    /// stems via `EventKind::DeckLoadStems`. Indexed by canonical stem
    /// order — 0=vocals, 1=drums, 2=bass, 3=other. Default
    /// `[1.0, 1.0, 1.0, 1.0]` means **all stems fully audible**, which
    /// is equivalent to the original full mix (stems sum to the input
    /// signal because demucs is designed that way). Setting any entry
    /// to 0.0 mutes that stem (e.g. `[0, 1, 1, 1]` = full mix minus
    /// vocals, the classic karaoke trick). Values are clamped to
    /// `[0.0, 1.0]` by the reducer.
    ///
    /// This field is ALWAYS populated (irrespective of whether the
    /// deck is in stem mode) so old serde payloads that omit it still
    /// deserialize. The audio thread only consults `stem_gains` when
    /// a stem handle is bound to the deck — in regular full-mix
    /// playback the field is ignored.
    #[serde(default = "default_stem_gains")]
    pub stem_gains: [f32; 4],
    /// Marker that this deck is in stem-mode (vs. full-mix mode).
    /// `true` after a successful `DeckLoadStems` reducer pass; cleared
    /// by `DeckLoad`, `DeckUnload`, or a fresh `DeckLoadStems`. The
    /// audio thread does not consult this — the mixer dispatches on
    /// its own `stem_handle: Option<StemHandle>`. State-side flag
    /// exists so a snapshot consumer (UI, persistence) can render the
    /// correct controls.
    #[serde(default)]
    pub stem_mode: bool,
}

impl Default for Deck {
    fn default() -> Self {
        // Hand-written `Default` because `tempo_ratio` defaults to 1.0
        // (not 0.0). Every other field uses its type's natural default;
        // capturing them explicitly avoids drift if a field is added
        // upstream without a matching default-handler edit here.
        Self {
            loaded: None,
            playing: false,
            position_ms: 0,
            pitch_semitones: 0.0,
            tempo_ratio: default_tempo_ratio(),
            eq_low_db: 0.0,
            eq_mid_db: 0.0,
            eq_high_db: 0.0,
            loop_in_ms: None,
            loop_out_ms: None,
            loop_active: false,
            hot_cues: [None; 8],
            copilot_engaged: false,
            track_gain_db: 0.0,
            bpm: 0.0,
            beat_grid_anchor_ms: 0,
            beat_period_ms: 0.0,
            phase_offset_ms: 0,
            downbeats: DownbeatGrid::new(),
            effects: Default::default(),
            handoff_until_frame: 0,
            stem_gains: default_stem_gains(),
            stem_mode: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EffectSlot {
    /// Effect-registry ID. 0 = empty slot.
    pub effect_id: u32,
    /// Param name → value. BTreeMap for deterministic ordering across forks.
    pub params: std::collections::BTreeMap<String, f32>,
    /// 0.0 = dry, 1.0 = full wet.
    pub wet_dry: f32,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EngineState {
    pub deck_a: Deck,
    pub deck_b: Deck,
    pub crossfader: f32, // 0.0 = full A, 1.0 = full B
    /// Crossfader response curve (pro-DJ convention). See
    /// [`CrossfaderCurve`] docs. Default = `Linear` for wire-compat
    /// (old snapshots without this field deserialize to existing
    /// behaviour).
    #[serde(default)]
    pub crossfader_curve: CrossfaderCurve,
    pub master_volume_db: f32,
    pub session_active: bool,
    /// Session master BPM (ADR-007 §v0.1). Drives MIDI clock OUT period.
    /// Default 120.0, updated by `EventKind::SetMasterBpm`.
    #[serde(default = "default_master_bpm")]
    pub master_bpm: f32,
    /// Master-bus soft-clip limiter — toggle. Default `true` so the
    /// live mix + recorded `master.wav` are protected against clipping
    /// from the moment the engine starts. See [`crate::audio::limiter`].
    #[serde(default = "default_master_limiter_enabled")]
    pub master_limiter_enabled: bool,
    /// Master-bus soft-clip limiter — threshold in dB. Default `-0.5`
    /// (linear ≈ 0.944). Reducer clamps incoming values to the
    /// `[-24.0, 0.0]` window via
    /// `audio::clamp_master_limiter_threshold_db`.
    #[serde(default = "default_master_limiter_threshold_db")]
    pub master_limiter_threshold_db: f32,
}

fn default_master_bpm() -> f32 {
    120.0
}

fn default_master_limiter_enabled() -> bool {
    true
}

fn default_master_limiter_threshold_db() -> f32 {
    crate::audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB
}

/// Serde default for `Deck::tempo_ratio` — 1.0 = original playback
/// speed. Lives next to `default_master_bpm` so both serde defaults are
/// findable together.
fn default_tempo_ratio() -> f32 {
    1.0
}

/// Serde default for `EventKind::DeckLoad.hot_cues` — 8 empty slots.
/// Used when an old payload (pre hot-cue persistence PR) omits the
/// field; the reducer then leaves the deck's cue grid untouched of
/// new entries. Tuple-style `[None; 8]` would require `Copy` semantics
/// the function form sidesteps cleanly.
fn default_hot_cues() -> [Option<u64>; 8] {
    [None; 8]
}

/// Serde + reducer default for `Deck::stem_gains` — `[1.0, 1.0, 1.0, 1.0]`.
/// All four stems fully audible = audibly equivalent to the original
/// full mix (demucs's vocals+drums+bass+other sum to ≈ the input,
/// modulo separation residual).
fn default_stem_gains() -> [f32; 4] {
    [1.0, 1.0, 1.0, 1.0]
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            deck_a: Deck::default(),
            deck_b: Deck::default(),
            crossfader: 0.5,
            crossfader_curve: CrossfaderCurve::default(),
            master_volume_db: 0.0,
            session_active: false,
            master_bpm: default_master_bpm(),
            master_limiter_enabled: default_master_limiter_enabled(),
            master_limiter_threshold_db: default_master_limiter_threshold_db(),
        }
    }
}

impl EngineState {
    /// Apply an event to produce a new state. Pure function — never mutates self.
    pub fn apply(&self, ev: &Event) -> Self {
        let mut next = self.clone();
        match &ev.kind {
            EventKind::SessionStart => next.session_active = true,
            EventKind::SessionEnd => next.session_active = false,
            EventKind::DeckLoad {
                deck: id,
                track,
                bpm,
                beat_grid_anchor_ms,
                downbeats_ms,
                hot_cues,
                track_gain_db,
            } => {
                let d = next.deck_mut(*id);
                d.loaded = Some(track.clone());
                d.position_ms = 0;
                d.playing = false;
                let safe_bpm = if bpm.is_finite() && *bpm > 0.0 {
                    *bpm
                } else {
                    120.0
                };
                d.bpm = safe_bpm;
                d.beat_grid_anchor_ms = *beat_grid_anchor_ms;
                d.beat_period_ms = 60_000.0 / safe_bpm;
                d.phase_offset_ms = 0;
                // Truncate to inline capacity. SmallVec::from_iter spills to
                // heap only when input > inline cap, but we cap explicitly so
                // the audio-thread snapshot is bounded regardless of input
                // size — a malicious or buggy copilot can't blow up `Deck`
                // size via a giant downbeats array.
                let take = downbeats_ms.len().min(DOWNBEATS_INLINE_CAPACITY);
                d.downbeats = DownbeatGrid::from_slice(&downbeats_ms[..take]);
                // Replace the per-deck hot-cue grid wholesale. Loading
                // a new track always overwrites any in-memory cues
                // (matches the prior "DeckLoad replaces deck state"
                // contract); pre-PR payloads come in with all-None
                // via the serde default and so behave exactly like
                // before.
                d.hot_cues = *hot_cues;
                // Loudness leveler — defensive against non-finite
                // payloads. The copilot side already clamps to
                // `[-20, +14]` dB but a buggy / malicious bridge
                // client could ship a NaN, which would propagate
                // into a NaN multiply on every audio sample. Treat
                // non-finite as 0 dB (= passthrough). No upper / lower
                // clamp here — that's the copilot's responsibility,
                // and we'd rather a too-loud value land at the master
                // limiter than silently re-shape the user's request.
                d.track_gain_db = if track_gain_db.is_finite() {
                    *track_gain_db
                } else {
                    0.0
                };
                // Full-mix load clears stem-mode (mutually exclusive
                // with DeckLoadStems). Reset stem_gains to default
                // so a later stem-load on the same deck starts from
                // the documented all-audible baseline.
                d.stem_mode = false;
                d.stem_gains = default_stem_gains();
            }
            EventKind::DeckLoadStems {
                deck: id,
                track,
                stem_paths: _,
                bpm,
                beat_grid_anchor_ms,
                downbeats_ms,
                hot_cues,
            } => {
                // Stem-mode load. Shares the beatgrid + hot-cue
                // payload shape with `DeckLoad` so the copilot can
                // reuse the analyzer pass it already ran on the full
                // mix (stems are derived from the same source). The
                // `stem_paths` themselves are consumed by the
                // translator (it opens each path via the decode
                // service); the reducer doesn't store them on the
                // deck because the audio thread reads from the
                // opaque `StemHandle` instead.
                let d = next.deck_mut(*id);
                d.loaded = Some(track.clone());
                d.position_ms = 0;
                d.playing = false;
                let safe_bpm = if bpm.is_finite() && *bpm > 0.0 {
                    *bpm
                } else {
                    120.0
                };
                d.bpm = safe_bpm;
                d.beat_grid_anchor_ms = *beat_grid_anchor_ms;
                d.beat_period_ms = 60_000.0 / safe_bpm;
                d.phase_offset_ms = 0;
                let take = downbeats_ms.len().min(DOWNBEATS_INLINE_CAPACITY);
                d.downbeats = DownbeatGrid::from_slice(&downbeats_ms[..take]);
                d.hot_cues = *hot_cues;
                // Stem-mode marker + reset stem gains to the
                // all-audible baseline so a stale `SetStemGain` from
                // a previous track can't carry over.
                d.stem_mode = true;
                d.stem_gains = default_stem_gains();
            }
            EventKind::SetStemGain {
                deck: id,
                stem,
                gain,
            } => {
                // Out-of-range stem index is a silent no-op (mirrors
                // the reducer's defensive style on HotCueSet /
                // EffectSwapSlots). Gain is clamped to [0, 1] to
                // protect the per-block stem mix MAC from
                // accidentally negative/saturating values.
                if (*stem as usize) < 4 {
                    next.deck_mut(*id).stem_gains[*stem as usize] = gain.clamp(0.0, 1.0);
                }
            }
            EventKind::DeckUnload { deck: id } => {
                *next.deck_mut(*id) = Deck::default();
            }
            EventKind::PhaseNudge { deck: id, delta_ms } => {
                let d = next.deck_mut(*id);
                d.phase_offset_ms = d.phase_offset_ms.saturating_add(*delta_ms);
            }
            EventKind::EffectAssign {
                deck: id,
                slot,
                effect_id,
            } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    *s = EffectSlot {
                        effect_id: *effect_id,
                        params: Default::default(),
                        wet_dry: 0.5,
                        enabled: true,
                    };
                }
            }
            EventKind::EffectClear { deck: id, slot } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    *s = EffectSlot::default();
                }
            }
            EventKind::EffectParam {
                deck: id,
                slot,
                param,
                value,
            } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    s.params.insert(param.clone(), *value);
                }
            }
            EventKind::EffectWetDry {
                deck: id,
                slot,
                value,
            } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    s.wet_dry = value.clamp(0.0, 1.0);
                }
            }
            EventKind::EffectEnable {
                deck: id,
                slot,
                enabled,
            } => {
                if let Some(s) = next.deck_mut(*id).effects.get_mut(*slot as usize) {
                    s.enabled = *enabled;
                }
            }
            EventKind::EffectSwapSlots {
                deck: id,
                slot_a,
                slot_b,
            } => {
                // Clamp both indices into 0..3. The slice's natural
                // upper bound (`effects.len() - 1`) is the safe ceiling
                // so out-of-range values land on the last valid slot
                // (matches the reducer's defensive style elsewhere —
                // see `HotCueSet` guarding).
                let last = (next.deck_mut(*id).effects.len() - 1) as u8;
                let a = (*slot_a).min(last) as usize;
                let b = (*slot_b).min(last) as usize;
                if a != b {
                    next.deck_mut(*id).effects.swap(a, b);
                }
            }
            EventKind::DeckPlay { deck: id } => {
                next.deck_mut(*id).playing = true;
            }
            EventKind::DeckPause { deck: id } => {
                next.deck_mut(*id).playing = false;
            }
            EventKind::DeckCue {
                deck: id,
                position_ms,
            } => {
                next.deck_mut(*id).position_ms = *position_ms;
            }
            EventKind::Crossfader { value } => {
                next.crossfader = value.clamp(0.0, 1.0);
            }
            EventKind::SetCrossfaderCurve { curve } => {
                next.crossfader_curve = *curve;
            }
            EventKind::EqAdjust {
                deck: id,
                band,
                value_db,
            } => {
                let clamped = value_db.clamp(-26.0, 6.0); // pro convention
                let d = next.deck_mut(*id);
                match band {
                    EqBand::Low => d.eq_low_db = clamped,
                    EqBand::Mid => d.eq_mid_db = clamped,
                    EqBand::High => d.eq_high_db = clamped,
                }
            }
            EventKind::HotCueSet {
                deck: id,
                slot,
                position_ms,
            } => {
                if (*slot as usize) < 8 {
                    next.deck_mut(*id).hot_cues[*slot as usize] = Some(*position_ms);
                }
            }
            EventKind::HotCueTrigger { deck: id, slot } => {
                if (*slot as usize) < 8 {
                    if let Some(pos) = next.deck(*id).hot_cues[*slot as usize] {
                        next.deck_mut(*id).position_ms = pos;
                    }
                }
            }
            EventKind::LoopIn { deck: id } => {
                let pos = next.deck(*id).position_ms;
                next.deck_mut(*id).loop_in_ms = Some(pos);
            }
            EventKind::LoopOut { deck: id } => {
                let pos = next.deck(*id).position_ms;
                let had_in = next.deck(*id).loop_in_ms.is_some();
                let d = next.deck_mut(*id);
                d.loop_out_ms = Some(pos);
                d.loop_active = had_in;
            }
            EventKind::LoopExit { deck: id } => {
                let d = next.deck_mut(*id);
                d.loop_in_ms = None;
                d.loop_out_ms = None;
                d.loop_active = false;
            }
            EventKind::PitchBend {
                deck: id,
                semitones,
            } => {
                next.deck_mut(*id).pitch_semitones =
                    crate::audio::clamp_pitch_semitones(*semitones);
            }
            EventKind::TempoBend { deck: id, ratio } => {
                // Use the audio module's clamp so the reducer and the
                // audio-thread `PitchTempo::set_tempo_ratio` apply the
                // exact same range — no risk of the state log holding
                // a value the audio path silently re-clamps differently.
                next.deck_mut(*id).tempo_ratio = crate::audio::clamp_tempo_ratio(*ratio);
            }
            EventKind::PitchTempoReset { deck: id } => {
                let d = next.deck_mut(*id);
                d.pitch_semitones = 0.0;
                d.tempo_ratio = default_tempo_ratio();
            }
            EventKind::CopilotEngage { deck: id } => {
                next.deck_mut(*id).copilot_engaged = true;
            }
            EventKind::CopilotDisengage { deck: id } => {
                next.deck_mut(*id).copilot_engaged = false;
            }
            EventKind::SetMasterBpm { bpm } => {
                if bpm.is_finite() && *bpm > 0.0 {
                    next.master_bpm = *bpm;
                }
                // Otherwise: no-op. The reducer is pure and the
                // SharedClock side ignores bad values too.
            }
            EventKind::TakeOver {
                deck: id,
                handoff_until_frame,
            } => {
                // User pre-empts copilot — disengage immediately + set
                // the 1-bar handoff window end per ADR-005. Audio
                // thread continues AI automation through this frame
                // while user inputs cross-fade in. Reducer stores the
                // value computed by the control thread; pure function.
                let d = next.deck_mut(*id);
                d.copilot_engaged = false;
                d.handoff_until_frame = *handoff_until_frame;
            }
            EventKind::SetMasterLimiterEnabled { enabled } => {
                next.master_limiter_enabled = *enabled;
            }
            EventKind::SetMasterLimiterThreshold { threshold_db } => {
                next.master_limiter_threshold_db =
                    crate::audio::clamp_master_limiter_threshold_db(*threshold_db);
            }
        }
        next
    }

    fn deck(&self, id: DeckId) -> &Deck {
        match id {
            DeckId::A => &self.deck_a,
            DeckId::B => &self.deck_b,
        }
    }

    fn deck_mut(&mut self, id: DeckId) -> &mut Deck {
        match id {
            DeckId::A => &mut self.deck_a,
            DeckId::B => &mut self.deck_b,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: u64, kind: EventKind) -> Event {
        Event {
            id,
            ts_micros: 0,
            source: EventSource::Ui,
            kind,
        }
    }

    #[test]
    fn deck_play_then_pause() {
        let s = EngineState::default();
        let s = s.apply(&ev(1, EventKind::DeckPlay { deck: DeckId::A }));
        assert!(s.deck_a.playing);
        let s = s.apply(&ev(2, EventKind::DeckPause { deck: DeckId::A }));
        assert!(!s.deck_a.playing);
    }

    #[test]
    fn crossfader_clamps() {
        let s = EngineState::default();
        let s = s.apply(&ev(1, EventKind::Crossfader { value: 2.5 }));
        assert_eq!(s.crossfader, 1.0);
        let s = s.apply(&ev(2, EventKind::Crossfader { value: -0.5 }));
        assert_eq!(s.crossfader, 0.0);
    }

    #[test]
    fn eq_clamps_to_pro_range() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::EqAdjust {
                deck: DeckId::A,
                band: EqBand::Low,
                value_db: -50.0,
            },
        ));
        assert_eq!(s.deck_a.eq_low_db, -26.0);
        let s = s.apply(&ev(
            2,
            EventKind::EqAdjust {
                deck: DeckId::B,
                band: EqBand::High,
                value_db: 99.0,
            },
        ));
        assert_eq!(s.deck_b.eq_high_db, 6.0);
    }

    #[test]
    fn hot_cue_set_then_trigger_seeks() {
        let s = EngineState::default();
        // Place deck A at 60000ms first
        let s = s.apply(&ev(
            1,
            EventKind::DeckCue {
                deck: DeckId::A,
                position_ms: 60000,
            },
        ));
        // Save hot cue to slot 0
        let s = s.apply(&ev(
            2,
            EventKind::HotCueSet {
                deck: DeckId::A,
                slot: 0,
                position_ms: 60000,
            },
        ));
        // Move somewhere else
        let s = s.apply(&ev(
            3,
            EventKind::DeckCue {
                deck: DeckId::A,
                position_ms: 90000,
            },
        ));
        assert_eq!(s.deck_a.position_ms, 90000);
        // Trigger hot cue 0 — should jump back to 60000
        let s = s.apply(&ev(
            4,
            EventKind::HotCueTrigger {
                deck: DeckId::A,
                slot: 0,
            },
        ));
        assert_eq!(s.deck_a.position_ms, 60000);
    }

    #[test]
    fn takeover_disengages_copilot_and_arms_handoff() {
        let s = EngineState::default();
        let s = s.apply(&ev(1, EventKind::CopilotEngage { deck: DeckId::A }));
        assert!(s.deck_a.copilot_engaged);
        let s = s.apply(&ev(
            2,
            EventKind::TakeOver {
                deck: DeckId::A,
                handoff_until_frame: 96_000, // ~2s at 48kHz = ~1 bar at 120 BPM
            },
        ));
        assert!(!s.deck_a.copilot_engaged);
        assert_eq!(s.deck_a.handoff_until_frame, 96_000);
    }

    #[test]
    fn apply_is_pure() {
        let s = EngineState::default();
        let _ = s.apply(&ev(1, EventKind::DeckPlay { deck: DeckId::A }));
        // Original must be untouched (clone semantics).
        assert!(!s.deck_a.playing);
    }

    #[test]
    fn deck_load_sets_beatgrid_and_tempo() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t1".into(),
                    path: "/tracks/x.mp3".into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 220,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_a.bpm, 128.0);
        assert_eq!(s.deck_a.beat_grid_anchor_ms, 220);
        assert!((s.deck_a.beat_period_ms - (60_000.0 / 128.0)).abs() < 0.001);
        assert!(s.deck_a.downbeats.is_empty());
    }

    #[test]
    fn deck_load_clamps_invalid_bpm_to_default() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t".into(),
                    path: "/p".into(),
                },
                bpm: f32::NAN,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_a.bpm, 120.0);
    }

    #[test]
    fn deck_unload_clears_state() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t".into(),
                    path: "/p".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        ));
        let s = s.apply(&ev(2, EventKind::DeckPlay { deck: DeckId::A }));
        assert!(s.deck_a.loaded.is_some());
        let s = s.apply(&ev(3, EventKind::DeckUnload { deck: DeckId::A }));
        assert!(s.deck_a.loaded.is_none());
        assert!(!s.deck_a.playing);
        assert!(s.deck_a.downbeats.is_empty());
    }

    #[test]
    fn deck_load_populates_downbeats() {
        let s = EngineState::default();
        // 4-bar grid at 120 BPM: bar = 4 × 500ms = 2000ms.
        let downbeats: Vec<u32> = (0..10).map(|i| i * 2000).collect();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::B,
                track: TrackRef {
                    id: "tdb".into(),
                    path: "/tracks/db.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: downbeats.clone(),
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_b.downbeats.len(), downbeats.len());
        assert_eq!(s.deck_b.downbeats[0], 0);
        assert_eq!(s.deck_b.downbeats[9], 18_000);
        // Per-track grid only — deck A must remain untouched.
        assert!(s.deck_a.downbeats.is_empty());
    }

    #[test]
    fn deck_load_truncates_downbeats_at_inline_capacity() {
        let s = EngineState::default();
        // Synthesize 200 downbeats — well above the inline cap of 64.
        let downbeats: Vec<u32> = (0..200u32).map(|i| i * 2000).collect();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "huge".into(),
                    path: "/tracks/huge.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: downbeats,
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_a.downbeats.len(), DOWNBEATS_INLINE_CAPACITY);
        // First 64 entries are preserved (FIFO truncation per the docstring).
        assert_eq!(s.deck_a.downbeats[0], 0);
        assert_eq!(
            s.deck_a.downbeats[DOWNBEATS_INLINE_CAPACITY - 1],
            ((DOWNBEATS_INLINE_CAPACITY - 1) as u32) * 2000,
        );
    }

    #[test]
    fn deck_load_replacing_track_resets_downbeats() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t1".into(),
                    path: "/a.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![0, 2000, 4000, 6000],
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_a.downbeats.len(), 4);
        // Replacing track on same deck must overwrite the grid wholesale, not
        // append. Otherwise stale downbeats from the previous track could
        // confuse the proposer's `next_downbeat_after` math.
        let s = s.apply(&ev(
            2,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t2".into(),
                    path: "/b.mp3".into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 100,
                downbeats_ms: vec![100, 1975, 3850],
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_a.downbeats.len(), 3);
        assert_eq!(s.deck_a.downbeats[0], 100);
    }

    #[test]
    fn deck_load_populates_hot_cues_from_payload() {
        // Hot-cue persistence PR: DeckLoad now carries an 8-slot
        // hot-cue array (library → engine). The reducer copies it
        // verbatim onto the deck so a track always loads with the
        // cues it was last saved with.
        let s = EngineState::default();
        let cues = [
            Some(0_u64),
            Some(1_500),
            None,
            Some(8_000),
            None,
            None,
            Some(60_000),
            None,
        ];
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "with-cues".into(),
                    path: "/p.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: cues,
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_a.hot_cues, cues);
        // Per-deck only — deck B must be untouched.
        assert!(s.deck_b.hot_cues.iter().all(Option::is_none));
    }

    #[test]
    fn deck_load_default_hot_cues_is_all_none() {
        // Wire-compat: an old DeckLoad payload (pre hot-cue
        // persistence) deserializes with `hot_cues` defaulting to
        // all-None via serde. Construct one explicitly here to mirror
        // that semantic (the default function under test).
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::B,
                track: TrackRef {
                    id: "no-cues".into(),
                    path: "/p.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: super::default_hot_cues(),
                track_gain_db: 0.0,
            },
        ));
        assert!(s.deck_b.hot_cues.iter().all(Option::is_none));
    }

    #[test]
    fn deck_load_replaces_existing_hot_cues_wholesale() {
        // Loading a *new* track on a deck must overwrite any prior
        // hot-cues — otherwise stale cues from the previous track
        // would phantom-trigger when the user hits a pad on the new
        // track. Mirrors the same contract as `downbeats` reset.
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t1".into(),
                    path: "/a.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [
                    Some(1_000),
                    Some(2_000),
                    Some(3_000),
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_a.hot_cues[0], Some(1_000));
        // Load a different track with a different cue layout — old
        // values must NOT persist.
        let s = s.apply(&ev(
            2,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t2".into(),
                    path: "/b.mp3".into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None, None, None, None, None, None, None, Some(99_000)],
                track_gain_db: 0.0,
            },
        ));
        assert_eq!(s.deck_a.hot_cues[0], None);
        assert_eq!(s.deck_a.hot_cues[7], Some(99_000));
    }

    #[test]
    fn deck_load_hot_cues_serde_roundtrip_with_default() {
        // Serde-default behaviour: an *omitted* `hot_cues` field in
        // a JSON payload still deserializes (default = all-None).
        // This catches accidental removal of `#[serde(default = ...)]`.
        let json = r#"{
            "DeckLoad": {
                "deck": "A",
                "track": { "id": "t1", "path": "/p.mp3" },
                "bpm": 120.0,
                "beat_grid_anchor_ms": 0
            }
        }"#;
        let kind: EventKind = serde_json::from_str(json).expect("deserialize");
        match kind {
            EventKind::DeckLoad { hot_cues, .. } => {
                assert!(hot_cues.iter().all(Option::is_none));
            }
            other => panic!("expected DeckLoad, got {other:?}"),
        }
    }

    #[test]
    fn deck_load_propagates_track_gain_db_to_deck() {
        // Loudness leveler: the per-track gain on the DeckLoad
        // payload lands on `Deck::track_gain_db` so a snapshot
        // consumer + the mixer's command translator can both see
        // it. Positive (boost) and negative (cut) both round-trip.
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "quiet".into(),
                    path: "/q.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
                track_gain_db: 9.0, // quiet jazz at -23 LUFS
            },
        ));
        assert!((s.deck_a.track_gain_db - 9.0).abs() < f32::EPSILON);
        // Deck B unaffected.
        assert_eq!(s.deck_b.track_gain_db, 0.0);

        // Negative (loud master needs cutting) — same plumbing.
        let s = s.apply(&ev(
            2,
            EventKind::DeckLoad {
                deck: DeckId::B,
                track: TrackRef {
                    id: "loud".into(),
                    path: "/l.mp3".into(),
                },
                bpm: 128.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
                track_gain_db: -6.0,
            },
        ));
        assert!((s.deck_b.track_gain_db - (-6.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn deck_load_track_gain_db_serde_default_is_zero() {
        // Wire compat: an old payload (pre-loudness-PR) without the
        // `track_gain_db` field deserializes with 0 dB = passthrough.
        let json = r#"{
            "DeckLoad": {
                "deck": "A",
                "track": { "id": "t1", "path": "/p.mp3" },
                "bpm": 120.0,
                "beat_grid_anchor_ms": 0
            }
        }"#;
        let kind: EventKind = serde_json::from_str(json).expect("deserialize");
        match kind {
            EventKind::DeckLoad { track_gain_db, .. } => {
                assert_eq!(track_gain_db, 0.0);
            }
            other => panic!("expected DeckLoad, got {other:?}"),
        }
    }

    #[test]
    fn deck_load_non_finite_track_gain_db_clamps_to_zero() {
        // Defensive: a buggy / malicious copilot payload could ship
        // NaN, which would propagate into a NaN multiply on every
        // audio sample. Reducer guard maps non-finite → 0 dB.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let s = EngineState::default();
            let s = s.apply(&ev(
                1,
                EventKind::DeckLoad {
                    deck: DeckId::A,
                    track: TrackRef {
                        id: "t".into(),
                        path: "/p.mp3".into(),
                    },
                    bpm: 120.0,
                    beat_grid_anchor_ms: 0,
                    downbeats_ms: vec![],
                    hot_cues: [None; 8],
                    track_gain_db: bad,
                },
            ));
            assert_eq!(
                s.deck_a.track_gain_db, 0.0,
                "non-finite payload {bad} must be filtered"
            );
        }
    }

    #[test]
    fn phase_nudge_accumulates() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::PhaseNudge {
                deck: DeckId::A,
                delta_ms: 10,
            },
        ));
        let s = s.apply(&ev(
            2,
            EventKind::PhaseNudge {
                deck: DeckId::A,
                delta_ms: -3,
            },
        ));
        assert_eq!(s.deck_a.phase_offset_ms, 7);
    }

    #[test]
    fn effect_assign_and_param_clamp_wet_dry() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::EffectAssign {
                deck: DeckId::A,
                slot: 0,
                effect_id: 1,
            },
        ));
        let s = s.apply(&ev(
            2,
            EventKind::EffectParam {
                deck: DeckId::A,
                slot: 0,
                param: "cutoff_hz".into(),
                value: 500.0,
            },
        ));
        let s = s.apply(&ev(
            3,
            EventKind::EffectWetDry {
                deck: DeckId::A,
                slot: 0,
                value: 2.5,
            },
        ));
        assert_eq!(s.deck_a.effects[0].effect_id, 1);
        assert_eq!(s.deck_a.effects[0].params.get("cutoff_hz"), Some(&500.0));
        assert_eq!(s.deck_a.effects[0].wet_dry, 1.0); // clamped
    }

    // ADR-006 — slot reordering. Helper that assigns three different
    // effects to slots 0/1/2 + tweaks each so the swap can be verified
    // against full slot contents (effect_id + params + wet_dry +
    // enabled), not just effect_id.
    fn populate_three_distinct_slots() -> EngineState {
        let s = EngineState::default();
        // Slot 0: filter, cutoff_hz=500, wet=0.3, enabled (default after assign).
        let s = s.apply(&ev(
            1,
            EventKind::EffectAssign {
                deck: DeckId::A,
                slot: 0,
                effect_id: 1,
            },
        ));
        let s = s.apply(&ev(
            2,
            EventKind::EffectParam {
                deck: DeckId::A,
                slot: 0,
                param: "cutoff_hz".into(),
                value: 500.0,
            },
        ));
        let s = s.apply(&ev(
            3,
            EventKind::EffectWetDry {
                deck: DeckId::A,
                slot: 0,
                value: 0.3,
            },
        ));
        // Slot 1: echo, wet=0.6.
        let s = s.apply(&ev(
            4,
            EventKind::EffectAssign {
                deck: DeckId::A,
                slot: 1,
                effect_id: 2,
            },
        ));
        let s = s.apply(&ev(
            5,
            EventKind::EffectWetDry {
                deck: DeckId::A,
                slot: 1,
                value: 0.6,
            },
        ));
        // Slot 2: reverb, disabled (override the assign-default `true`).
        let s = s.apply(&ev(
            6,
            EventKind::EffectAssign {
                deck: DeckId::A,
                slot: 2,
                effect_id: 3,
            },
        ));
        s.apply(&ev(
            7,
            EventKind::EffectEnable {
                deck: DeckId::A,
                slot: 2,
                enabled: false,
            },
        ))
    }

    #[test]
    fn effect_swap_slots_swaps_full_contents() {
        // ADR-006 — slot reorder must move effect_id + params +
        // wet_dry + enabled together, not just effect_id. Without that
        // the slot's user-tuned state would drop on every drag.
        let s = populate_three_distinct_slots();
        let before_0 = s.deck_a.effects[0].clone();
        let before_2 = s.deck_a.effects[2].clone();
        let s = s.apply(&ev(
            10,
            EventKind::EffectSwapSlots {
                deck: DeckId::A,
                slot_a: 0,
                slot_b: 2,
            },
        ));
        // Slot 0 now holds what was in slot 2 (reverb, disabled),
        // slot 2 holds the old slot 0 (filter + cutoff + wet=0.3).
        assert_eq!(s.deck_a.effects[0].effect_id, before_2.effect_id);
        assert_eq!(s.deck_a.effects[0].enabled, before_2.enabled);
        assert_eq!(s.deck_a.effects[0].wet_dry, before_2.wet_dry);
        assert_eq!(s.deck_a.effects[2].effect_id, before_0.effect_id);
        assert_eq!(s.deck_a.effects[2].enabled, before_0.enabled);
        assert_eq!(s.deck_a.effects[2].wet_dry, before_0.wet_dry);
        assert_eq!(
            s.deck_a.effects[2].params.get("cutoff_hz"),
            Some(&500.0),
            "params must travel with the slot during a swap"
        );
        // Untouched slot 1 (echo) stays put.
        assert_eq!(s.deck_a.effects[1].effect_id, 2);
        assert!((s.deck_a.effects[1].wet_dry - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn effect_swap_slots_out_of_range_clamps_to_last_slot() {
        // slot_a=99 → clamps to last valid (2). slot_b=1 unchanged.
        // Net effect: slot 2 ↔ slot 1.
        let s = populate_three_distinct_slots();
        let before_1 = s.deck_a.effects[1].clone();
        let before_2 = s.deck_a.effects[2].clone();
        let s = s.apply(&ev(
            10,
            EventKind::EffectSwapSlots {
                deck: DeckId::A,
                slot_a: 99,
                slot_b: 1,
            },
        ));
        assert_eq!(s.deck_a.effects[1].effect_id, before_2.effect_id);
        assert_eq!(s.deck_a.effects[2].effect_id, before_1.effect_id);
        // Slot 0 unaffected.
        assert_eq!(s.deck_a.effects[0].effect_id, 1);
    }

    #[test]
    fn effect_swap_slots_same_index_is_noop() {
        // a == b → state is bit-for-bit identical. Catches a regression
        // where the swap path destructively replaces the slot.
        let s = populate_three_distinct_slots();
        let before = s.clone();
        let s = s.apply(&ev(
            10,
            EventKind::EffectSwapSlots {
                deck: DeckId::A,
                slot_a: 1,
                slot_b: 1,
            },
        ));
        for i in 0..3 {
            assert_eq!(
                s.deck_a.effects[i].effect_id,
                before.deck_a.effects[i].effect_id
            );
            assert_eq!(
                s.deck_a.effects[i].enabled,
                before.deck_a.effects[i].enabled
            );
            assert_eq!(
                s.deck_a.effects[i].wet_dry,
                before.deck_a.effects[i].wet_dry
            );
            assert_eq!(s.deck_a.effects[i].params, before.deck_a.effects[i].params);
        }
    }

    #[test]
    fn effect_swap_slots_both_out_of_range_clamps_to_last_each_noop() {
        // Both indices clamp to the same last slot → same-slot noop.
        let s = populate_three_distinct_slots();
        let before = s.clone();
        let s = s.apply(&ev(
            10,
            EventKind::EffectSwapSlots {
                deck: DeckId::A,
                slot_a: 200,
                slot_b: 50,
            },
        ));
        for i in 0..3 {
            assert_eq!(
                s.deck_a.effects[i].effect_id,
                before.deck_a.effects[i].effect_id
            );
        }
    }

    #[test]
    fn effect_swap_slots_targets_correct_deck() {
        // Swapping on deck A must not touch deck B's chain.
        let s = populate_three_distinct_slots();
        // Mirror something onto deck B so we can verify it's untouched.
        let s = s.apply(&ev(
            10,
            EventKind::EffectAssign {
                deck: DeckId::B,
                slot: 0,
                effect_id: 4, // gate
            },
        ));
        let s = s.apply(&ev(
            11,
            EventKind::EffectSwapSlots {
                deck: DeckId::A,
                slot_a: 0,
                slot_b: 1,
            },
        ));
        // Deck A: slot 0 was filter (1), slot 1 was echo (2) → swapped.
        assert_eq!(s.deck_a.effects[0].effect_id, 2);
        assert_eq!(s.deck_a.effects[1].effect_id, 1);
        // Deck B untouched.
        assert_eq!(s.deck_b.effects[0].effect_id, 4);
        assert_eq!(s.deck_b.effects[1].effect_id, 0);
        assert_eq!(s.deck_b.effects[2].effect_id, 0);
    }

    #[test]
    fn deck_default_tempo_ratio_is_one() {
        // Pitch/tempo PR: tempo_ratio must default to 1.0, not the
        // f32::default() of 0.0. Catches a regression of the manual
        // `Default` impl reverting to the auto-derive.
        let d: Deck = Default::default();
        assert!((d.tempo_ratio - 1.0).abs() < f32::EPSILON);
        assert!(d.pitch_semitones.abs() < f32::EPSILON);
    }

    #[test]
    fn tempo_bend_sets_ratio_clamped() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 1.25,
            },
        ));
        assert!((s.deck_a.tempo_ratio - 1.25).abs() < 1e-6);
        // Over-range clamps to max.
        let s = s.apply(&ev(
            2,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 10.0,
            },
        ));
        assert!((s.deck_a.tempo_ratio - crate::audio::MAX_TEMPO_RATIO).abs() < 1e-6);
        // Under-range clamps to min.
        let s = s.apply(&ev(
            3,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 0.0,
            },
        ));
        assert!((s.deck_a.tempo_ratio - crate::audio::MIN_TEMPO_RATIO).abs() < 1e-6);
        // NaN safely falls back to 1.0.
        let s = s.apply(&ev(
            4,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: f32::NAN,
            },
        ));
        assert!((s.deck_a.tempo_ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pitch_tempo_reset_returns_both_to_defaults() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::PitchBend {
                deck: DeckId::A,
                semitones: 5.0,
            },
        ));
        let s = s.apply(&ev(
            2,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 1.5,
            },
        ));
        assert!((s.deck_a.pitch_semitones - 5.0).abs() < 1e-6);
        assert!((s.deck_a.tempo_ratio - 1.5).abs() < 1e-6);
        let s = s.apply(&ev(3, EventKind::PitchTempoReset { deck: DeckId::A }));
        assert!(s.deck_a.pitch_semitones.abs() < f32::EPSILON);
        assert!((s.deck_a.tempo_ratio - 1.0).abs() < f32::EPSILON);
        // Other deck untouched.
        assert!((s.deck_b.tempo_ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pitch_bend_does_not_modify_tempo_ratio() {
        // Pitch/tempo independence at the state-log level: bending pitch
        // never touches tempo_ratio, even when its previous value was
        // non-default.
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::TempoBend {
                deck: DeckId::A,
                ratio: 0.8,
            },
        ));
        assert!((s.deck_a.tempo_ratio - 0.8).abs() < 1e-6);
        let s = s.apply(&ev(
            2,
            EventKind::PitchBend {
                deck: DeckId::A,
                semitones: 7.0,
            },
        ));
        assert!((s.deck_a.pitch_semitones - 7.0).abs() < 1e-6);
        assert!(
            (s.deck_a.tempo_ratio - 0.8).abs() < 1e-6,
            "PitchBend must not touch tempo_ratio"
        );
    }

    #[test]
    fn master_limiter_enabled_by_default() {
        // Safety-first default — limiter ON the moment the engine starts
        // so a hot session can't clip the master bus or the recording.
        let s = EngineState::default();
        assert!(s.master_limiter_enabled);
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn set_master_limiter_enabled_toggles() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::SetMasterLimiterEnabled { enabled: false },
        ));
        assert!(!s.master_limiter_enabled);
        let s = s.apply(&ev(2, EventKind::SetMasterLimiterEnabled { enabled: true }));
        assert!(s.master_limiter_enabled);
    }

    #[test]
    fn set_master_limiter_threshold_clamps_to_window() {
        let s = EngineState::default();
        // Over-max → clamp to 0 dB.
        let s = s.apply(&ev(
            1,
            EventKind::SetMasterLimiterThreshold { threshold_db: 12.0 },
        ));
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_MAX_THRESHOLD_DB).abs()
                < 1e-6
        );
        // Under-min → clamp to -24 dB.
        let s = s.apply(&ev(
            2,
            EventKind::SetMasterLimiterThreshold {
                threshold_db: -100.0,
            },
        ));
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_MIN_THRESHOLD_DB).abs()
                < 1e-6
        );
        // NaN → default.
        let s = s.apply(&ev(
            3,
            EventKind::SetMasterLimiterThreshold {
                threshold_db: f32::NAN,
            },
        ));
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn engine_state_serde_roundtrip_preserves_limiter_fields() {
        // Wire-compat: an older snapshot (pre-limiter) deserializes
        // back to defaults via the `#[serde(default)]` attributes —
        // catches accidental removal of those serde defaults.
        let json = r#"{
            "deck_a": {
                "loaded": null, "playing": false, "position_ms": 0,
                "pitch_semitones": 0.0, "tempo_ratio": 1.0,
                "eq_low_db": 0.0, "eq_mid_db": 0.0, "eq_high_db": 0.0,
                "loop_in_ms": null, "loop_out_ms": null, "loop_active": false,
                "hot_cues": [null,null,null,null,null,null,null,null],
                "copilot_engaged": false, "bpm": 0.0, "beat_grid_anchor_ms": 0,
                "beat_period_ms": 0.0, "phase_offset_ms": 0,
                "effects": [{"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false},
                            {"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false},
                            {"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false}],
                "handoff_until_frame": 0
            },
            "deck_b": {
                "loaded": null, "playing": false, "position_ms": 0,
                "pitch_semitones": 0.0, "tempo_ratio": 1.0,
                "eq_low_db": 0.0, "eq_mid_db": 0.0, "eq_high_db": 0.0,
                "loop_in_ms": null, "loop_out_ms": null, "loop_active": false,
                "hot_cues": [null,null,null,null,null,null,null,null],
                "copilot_engaged": false, "bpm": 0.0, "beat_grid_anchor_ms": 0,
                "beat_period_ms": 0.0, "phase_offset_ms": 0,
                "effects": [{"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false},
                            {"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false},
                            {"effect_id":0,"params":{},"wet_dry":0.0,"enabled":false}],
                "handoff_until_frame": 0
            },
            "crossfader": 0.5,
            "master_volume_db": 0.0,
            "session_active": false
        }"#;
        let s: EngineState = serde_json::from_str(json).expect("old snapshot must deserialize");
        // Missing limiter fields fall back to the serde defaults.
        assert!(s.master_limiter_enabled);
        assert!(
            (s.master_limiter_threshold_db - crate::audio::MASTER_LIMITER_DEFAULT_THRESHOLD_DB)
                .abs()
                < 1e-6
        );
        // Crossfader curve default = Linear (pre-curve PR snapshots
        // must keep producing the same audio).
        assert_eq!(s.crossfader_curve, CrossfaderCurve::Linear);
    }

    #[test]
    fn crossfader_curve_defaults_to_linear() {
        // Existing engine behaviour must be preserved — no UI / wire
        // changes should silently flip a session's curve.
        let s = EngineState::default();
        assert_eq!(s.crossfader_curve, CrossfaderCurve::Linear);
    }

    #[test]
    fn set_crossfader_curve_event_applies() {
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::SetCrossfaderCurve {
                curve: CrossfaderCurve::Dipped,
            },
        ));
        assert_eq!(s.crossfader_curve, CrossfaderCurve::Dipped);
        let s = s.apply(&ev(
            2,
            EventKind::SetCrossfaderCurve {
                curve: CrossfaderCurve::Sharp,
            },
        ));
        assert_eq!(s.crossfader_curve, CrossfaderCurve::Sharp);
        let s = s.apply(&ev(
            3,
            EventKind::SetCrossfaderCurve {
                curve: CrossfaderCurve::Scratch,
            },
        ));
        assert_eq!(s.crossfader_curve, CrossfaderCurve::Scratch);
        // Round-trip back to Linear.
        let s = s.apply(&ev(
            4,
            EventKind::SetCrossfaderCurve {
                curve: CrossfaderCurve::Linear,
            },
        ));
        assert_eq!(s.crossfader_curve, CrossfaderCurve::Linear);
    }

    #[test]
    fn set_crossfader_curve_does_not_touch_crossfader_value() {
        // Switching the curve is metadata only — the slider position
        // (`crossfader`) is preserved across the curve toggle so the
        // user doesn't hear a level jump.
        let s = EngineState::default();
        let s = s.apply(&ev(1, EventKind::Crossfader { value: 0.7 }));
        let s = s.apply(&ev(
            2,
            EventKind::SetCrossfaderCurve {
                curve: CrossfaderCurve::Sharp,
            },
        ));
        assert!((s.crossfader - 0.7).abs() < f32::EPSILON);
        assert_eq!(s.crossfader_curve, CrossfaderCurve::Sharp);
    }

    #[test]
    fn crossfader_curve_serde_externally_tagged_variants() {
        // Wire-shape pin: serde-default external tag = bare variant
        // names. The UI submits `{ SetCrossfaderCurve: { curve: "Dipped" } }`
        // so any reorder / rename here would break the wire contract.
        let kind = EventKind::SetCrossfaderCurve {
            curve: CrossfaderCurve::Dipped,
        };
        let json = serde_json::to_string(&kind).expect("serialize");
        assert_eq!(json, r#"{"SetCrossfaderCurve":{"curve":"Dipped"}}"#);
        let parsed: EventKind = serde_json::from_str(&json).expect("roundtrip");
        match parsed {
            EventKind::SetCrossfaderCurve { curve } => {
                assert_eq!(curve, CrossfaderCurve::Dipped);
            }
            other => panic!("expected SetCrossfaderCurve, got {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // Stem-aware playback (engine-stem-aware-mixer PR)
    // ----------------------------------------------------------------

    fn stem_paths() -> [String; 4] {
        [
            "/cache/t1/vocals.wav".into(),
            "/cache/t1/drums.wav".into(),
            "/cache/t1/bass.wav".into(),
            "/cache/t1/other.wav".into(),
        ]
    }

    #[test]
    fn default_stem_gains_is_all_audible() {
        // Contract: a fresh deck has all four stem gains at 1.0 so
        // when stems are loaded (without any explicit SetStemGain
        // commands) playback is audibly equivalent to the full mix.
        let s = EngineState::default();
        assert_eq!(s.deck_a.stem_gains, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(s.deck_b.stem_gains, [1.0, 1.0, 1.0, 1.0]);
        assert!(!s.deck_a.stem_mode);
    }

    #[test]
    fn set_stem_gain_mutes_vocals() {
        // Canonical "kill the vocals" trick: stem index 0 = vocals,
        // gain 0 → only drums/bass/other audible.
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::SetStemGain {
                deck: DeckId::A,
                stem: 0,
                gain: 0.0,
            },
        ));
        assert_eq!(s.deck_a.stem_gains, [0.0, 1.0, 1.0, 1.0]);
        // Other deck untouched.
        assert_eq!(s.deck_b.stem_gains, [1.0; 4]);
        // Reducer clamps an out-of-range gain to [0, 1].
        let s = s.apply(&ev(
            2,
            EventKind::SetStemGain {
                deck: DeckId::A,
                stem: 1,
                gain: 2.5,
            },
        ));
        assert!((s.deck_a.stem_gains[1] - 1.0).abs() < f32::EPSILON);
        let s = s.apply(&ev(
            3,
            EventKind::SetStemGain {
                deck: DeckId::A,
                stem: 2,
                gain: -0.4,
            },
        ));
        assert!(s.deck_a.stem_gains[2].abs() < f32::EPSILON);
    }

    #[test]
    fn deck_load_stems_vs_deck_load_are_mutually_exclusive() {
        // DeckLoadStems sets stem_mode = true; a subsequent regular
        // DeckLoad on the same deck must reset it back to false and
        // restore the all-audible baseline. (And vice-versa.) This
        // mirrors the audio thread's `apply` contract — only one of
        // {full-mix DecodeHandle, StemHandle} is ever live per deck.
        let s = EngineState::default();
        let s = s.apply(&ev(
            1,
            EventKind::DeckLoadStems {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t1".into(),
                    path: "/tracks/t1.mp3".into(),
                },
                stem_paths: stem_paths(),
                bpm: 128.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
            },
        ));
        assert!(s.deck_a.stem_mode);
        assert_eq!(s.deck_a.stem_gains, [1.0; 4]);
        assert!((s.deck_a.bpm - 128.0).abs() < f32::EPSILON);
        // Mutate stem gains to make sure a fresh load resets them.
        let s = s.apply(&ev(
            2,
            EventKind::SetStemGain {
                deck: DeckId::A,
                stem: 0,
                gain: 0.0,
            },
        ));
        assert_eq!(s.deck_a.stem_gains[0], 0.0);
        // Now switch to a regular DeckLoad — stem_mode must drop.
        let s = s.apply(&ev(
            3,
            EventKind::DeckLoad {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t2".into(),
                    path: "/tracks/t2.mp3".into(),
                },
                bpm: 120.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
                track_gain_db: 0.0,
            },
        ));
        assert!(!s.deck_a.stem_mode);
        // Gains reset to all-audible so a follow-up DeckLoadStems
        // starts from the documented baseline.
        assert_eq!(s.deck_a.stem_gains, [1.0; 4]);
        // And back the other way: DeckLoadStems re-engages stem mode
        // even from a DeckLoad'd state.
        let s = s.apply(&ev(
            4,
            EventKind::DeckLoadStems {
                deck: DeckId::A,
                track: TrackRef {
                    id: "t3".into(),
                    path: "/tracks/t3.mp3".into(),
                },
                stem_paths: stem_paths(),
                bpm: 124.0,
                beat_grid_anchor_ms: 0,
                downbeats_ms: vec![],
                hot_cues: [None; 8],
            },
        ));
        assert!(s.deck_a.stem_mode);
    }

    #[test]
    fn set_stem_gain_out_of_range_index_is_silent_noop() {
        // Defensive: a malformed copilot suggestion (or stale UI)
        // could send a stem index of 4+ — the reducer must drop it
        // silently rather than panic / clobber an adjacent slot.
        let s = EngineState::default();
        let before = s.deck_a.stem_gains;
        for bad_stem in [4_u8, 7, 99, 255] {
            let s2 = s.apply(&ev(
                1,
                EventKind::SetStemGain {
                    deck: DeckId::A,
                    stem: bad_stem,
                    gain: 0.0,
                },
            ));
            assert_eq!(
                s2.deck_a.stem_gains, before,
                "out-of-range stem {bad_stem} mutated the gains array",
            );
        }
    }
}
