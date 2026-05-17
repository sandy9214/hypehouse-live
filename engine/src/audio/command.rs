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

use crate::state::DeckId;

/// Maximum on-wire fixed-buffer length we allow inside an `AudioCommand`.
/// Currently unused by any variant (no string-bearing variants ship in
/// v0.1) but kept as a guardrail: any future variant that wants
/// per-command opaque metadata must use a `[u8; RAMP_BUFFER_MAX]` rather
/// than a slice / Vec / String.
pub const RAMP_BUFFER_MAX: usize = 32;

/// Opaque handle into the audio thread's pre-decoded buffer registry.
/// Indexed lookup, never dereferenced into a pointer on the audio
/// thread — the registry holds `Arc<DecodedTrack>` and the audio thread
/// reads them via a lock-free index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferId(pub u32);

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
    Pitch {
        deck: DeckId,
        semitones: f32,
        ramp_frames: u32,
    },
    LoopArm {
        deck: DeckId,
        in_frame: u64,
        out_frame: u64,
    },
    LoopDisarm {
        deck: DeckId,
    },
    /// The pre-decoded buffer is now ready; bind it to this deck so the
    /// audio thread can render from it.
    DeckLoadBuffer {
        deck: DeckId,
        buffer_id: BufferId,
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
        assert_bounds::<BufferId>();
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
