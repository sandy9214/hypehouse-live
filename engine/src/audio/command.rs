//! AudioCommand — the wire format from control thread → audio thread.
//!
//! ADR-004 §"AudioCommand shape (v0)" defines the contract. Every variant
//! must be `Copy + Send + Sync + 'static` so it fits in a fixed-size SPSC
//! ring slot without heap allocation.
//!
//! Hard constraints:
//! * NO `String`, `Vec`, `HashMap`, or any heap-backed type.
//! * Fixed-size buffers (`[u8; N]`) + indices into a separate registry are
//!   used wherever variable-length data would otherwise be needed.
//! * `effect_id`, `buffer_id`, etc. are integer handles. The actual
//!   `Arc<DecodedTrack>` etc. lives in a registry the audio thread reads
//!   lock-free by index.

use crate::audio::decode::DecodeHandle;
use crate::state::DeckId;

/// Maximum on-wire fixed-buffer length we allow inside an `AudioCommand`.
/// Currently unused by any variant (no string-bearing variants ship in
/// v0.1) but kept as a guardrail: any future variant that wants
/// per-command opaque metadata must use a `[u8; RAMP_BUFFER_MAX]` rather
/// than a slice / Vec / String.
pub const RAMP_BUFFER_MAX: usize = 32;

/// Legacy alias kept for the `audio_command_is_copy_send_sync_static`
/// test and any out-of-tree callers still importing `BufferId`. The
/// streaming decode pipeline uses `DecodeHandle` directly inside
/// `AudioCommandKind::DeckLoad`. This alias is `#[deprecated]` to flag
/// downstream callers without breaking the build.
#[deprecated(note = "use DecodeHandle from crate::audio::decode")]
pub type BufferId = DecodeHandle;

/// A command applied at an absolute sample frame (engine clock).
///
/// Pure POD — `Copy + Send + Sync + 'static`. Goes through the SPSC ring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioCommand {
    /// Absolute sample frame this command should take effect at. The
    /// audio thread drains commands where `at_frame <= end_of_buffer`.
    /// "Now" = next buffer boundary = current engine clock.
    pub at_frame: u64,
    pub kind: AudioCommandKind,
}

/// All command variants the control thread can emit. Every field is
/// `Copy` POD.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioCommandKind {
    DeckPlay {
        deck: DeckId,
    },
    DeckPause {
        deck: DeckId,
    },
    DeckSeek {
        deck: DeckId,
        frame: u64,
    },
    /// Smooth crossfader ramp. `ramp_frames > 0` prevents zipper noise.
    Crossfader {
        target: f32,
        ramp_frames: u32,
    },
    EqLow {
        deck: DeckId,
        target_db: f32,
        ramp_frames: u32,
    },
    EqMid {
        deck: DeckId,
        target_db: f32,
        ramp_frames: u32,
    },
    EqHigh {
        deck: DeckId,
        target_db: f32,
        ramp_frames: u32,
    },
    /// Set the per-deck **pure pitch shift** (independent of tempo,
    /// post the pitch/tempo-independent PR). Drives stage 1 of the
    /// `audio::pitch_tempo::PitchTempo` cascade.
    Pitch {
        deck: DeckId,
        semitones: f32,
        ramp_frames: u32,
    },
    /// Set the per-deck **tempo ratio** (1.0 = original speed,
    /// independent of pitch). Drives stage 2 of the cascade.
    Tempo {
        deck: DeckId,
        ratio: f32,
        ramp_frames: u32,
    },
    /// Reset both pitch + tempo on a deck to defaults (0 semitones /
    /// 1.0 ratio) and clear the rubato cascade's internal state.
    PitchTempoReset {
        deck: DeckId,
    },
    LoopArm {
        deck: DeckId,
        in_frame: u64,
        out_frame: u64,
    },
    LoopDisarm {
        deck: DeckId,
    },
    /// Streaming decode handle is now ready; bind it to this deck so
    /// the audio thread can pull frames from it via
    /// `DecodeService::read`.
    DeckLoad {
        deck: DeckId,
        handle: DecodeHandle,
    },
    DeckUnload {
        deck: DeckId,
    },
    /// ADR-005 — co-pilot takeover handoff window arm. The audio thread
    /// continues the AI's last-emitted automation envelopes until
    /// `until_frame`, while user inputs cross-fade in. Pure metadata —
    /// no immediate audio side effect.
    ArmHandoff {
        deck: DeckId,
        until_frame: u64,
    },
    /// Cancel any pending command on `deck` whose `at_frame > now_frame +
    /// after_frames`. Used by `TakeOver` to abort queued AI commands.
    CancelAfter {
        deck: DeckId,
        after_frames: u32,
    },
    /// ADR-006 — assign effect to a deck's slot. `effect_id` 0 = clear.
    /// `slot` is 0..3 (per-deck chain).
    EffectAssign {
        deck: DeckId,
        slot: u8,
        effect_id: u32,
    },
    /// ADR-006 — clear an effect slot (return it to passthrough). The
    /// translator emits this for `EffectClear` events; `EffectAssign`
    /// with id=0 also clears.
    EffectClear {
        deck: DeckId,
        slot: u8,
    },
    /// ADR-006 — set one effect param by **numeric** index. The
    /// translator resolves the event's textual param name into an
    /// index via the registry's `resolve_param`, so no String reaches
    /// the audio thread.
    EffectParam {
        deck: DeckId,
        slot: u8,
        param_id: u8,
        value: f32,
    },
    /// ADR-006 — set the wet/dry blend for a slot (0..1).
    EffectWetDry {
        deck: DeckId,
        slot: u8,
        value: f32,
    },
    /// ADR-006 — enable/disable a slot in place (without losing state).
    EffectEnable {
        deck: DeckId,
        slot: u8,
        enabled: bool,
    },
    /// Master-bus soft-clip limiter — toggle bypass. `enabled = false`
    /// short-circuits the limiter's process loop to a no-op, zero CPU.
    /// See [`crate::audio::limiter`].
    SetMasterLimiterEnabled {
        enabled: bool,
    },
    /// Master-bus soft-clip limiter — set the threshold in dB. The
    /// control side already clamps to
    /// `[MASTER_LIMITER_MIN_THRESHOLD_DB, MASTER_LIMITER_MAX_THRESHOLD_DB]`;
    /// the audio thread defensive-clamps a second time.
    SetMasterLimiterThreshold {
        threshold_db: f32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time assertion that `AudioCommand` is exactly
    /// `Copy + Send + Sync + 'static`. If the enum grows a non-`Copy`
    /// field (e.g., someone adds a `String`), this fails to compile —
    /// catching the ADR-004 violation at build time.
    #[test]
    fn audio_command_is_copy_send_sync_static() {
        fn assert_bounds<T: Copy + Send + Sync + 'static>() {}
        assert_bounds::<AudioCommand>();
        assert_bounds::<AudioCommandKind>();
        assert_bounds::<DecodeHandle>();
    }

    #[test]
    fn command_size_is_bounded() {
        // Sanity: command should be small enough to fit cheaply in a
        // ring slot. 64 bytes is a generous upper bound on x86_64 /
        // aarch64; raise this only if a real new variant demands it.
        assert!(
            core::mem::size_of::<AudioCommand>() <= 64,
            "AudioCommand grew to {} bytes — review ADR-004 ring sizing",
            core::mem::size_of::<AudioCommand>()
        );
    }
}
