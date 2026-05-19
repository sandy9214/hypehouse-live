//! Per-deck effects chain (ADR-006).
//!
//! Each deck owns 3 effect slots; each slot can host one of 4 built-in
//! effects (Filter, Echo, Reverb, Gate). Effects run in slot order,
//! after the deck's source samples are pulled, before the crossfader.
//!
//! Hard rules (ADR-004 / ADR-006):
//! * Audio-thread side `process()` MUST NOT allocate.
//! * No `unsafe`, no Mutex, no blocking primitives.
//! * Param updates flow via `AudioCommand::EffectParam` (POD: numeric
//!   `param_id` index, not a `String`). The translator resolves the
//!   event's param name into a numeric id by asking the registry.
//!
//! Pre-allocation strategy (option (a) per spec): each slot pre-builds
//! all 4 effect structs and dispatches by the currently-assigned
//! `effect_id`. State of unassigned effects is irrelevant — `reset()`
//! is called on the target when a slot is re-assigned.

use crate::audio::clock::SharedClock;

pub mod echo;
pub mod filter;
pub mod gate;
pub mod lfo;
pub mod reverb;

pub use echo::Echo;
pub use filter::Filter;
pub use gate::Gate;
pub use lfo::{apply_lfo_to_params, Lfo, LfoConfig, RateDiv, Shape};
pub use reverb::Reverb;

/// Numeric effect id reserved in the registry. 0 = empty slot.
pub type EffectId = u32;
pub const EFFECT_NONE: EffectId = 0;
pub const EFFECT_FILTER: EffectId = 1;
pub const EFFECT_ECHO: EffectId = 2;
pub const EFFECT_REVERB: EffectId = 3;
pub const EFFECT_GATE: EffectId = 4;

/// Maximum number of params any effect can expose. Chosen at 6 so the
/// fixed-size param table fits without heap allocation.
pub const MAX_PARAMS: usize = 6;

/// Per-effect parameter table. Fixed `[f32; MAX_PARAMS]` keyed by the
/// effect's `params()` slice index. Audio-thread side reads only; the
/// control thread mutates via the translator-emitted command.
pub type EffectParams = [f32; MAX_PARAMS];

/// Static descriptor of one effect parameter. The registry exposes
/// these so the UI manifest / translator can resolve param names →
/// indices without runtime allocation.
#[derive(Clone, Copy, Debug)]
pub struct ParamDescriptor {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

impl ParamDescriptor {
    /// Clamp a candidate value into the descriptor's range.
    #[inline]
    pub fn clamp(&self, v: f32) -> f32 {
        v.clamp(self.min, self.max)
    }
}

/// The Effect trait. **Audio-thread side**.
///
/// `process` is the realtime hot path: must not allocate, must not
/// lock, must not panic, must not call back into the registry.
pub trait Effect: Send + Sync {
    fn id(&self) -> EffectId;
    fn name(&self) -> &'static str;
    fn params(&self) -> &'static [ParamDescriptor];
    /// In-place process. **MUST NOT allocate**. `buf` is interleaved
    /// **stereo** (L, R, L, R, …). `wet_dry` ∈ [0, 1].
    fn process(&mut self, buf: &mut [f32], params: &EffectParams, wet_dry: f32, sample_rate: u32);
    /// Clear internal state (delay lines, filter z's, gate phase…).
    fn reset(&mut self);
}

/// Resolve a textual param name to a numeric index for the given
/// effect id. Returns `None` if the effect or param is unknown.
///
/// **Control-thread side only.** Lives here (not on the trait) so it
/// stays out of the audio-thread codepath.
pub fn resolve_param(effect_id: EffectId, name: &str) -> Option<u8> {
    let descs: &[ParamDescriptor] = match effect_id {
        EFFECT_FILTER => Filter::DESCRIPTORS,
        EFFECT_ECHO => Echo::DESCRIPTORS,
        EFFECT_REVERB => Reverb::DESCRIPTORS,
        EFFECT_GATE => Gate::DESCRIPTORS,
        _ => return None,
    };
    descs.iter().position(|d| d.name == name).map(|i| i as u8)
}

/// Look up an effect's descriptor list by id. Control-thread side.
pub fn descriptors(effect_id: EffectId) -> &'static [ParamDescriptor] {
    match effect_id {
        EFFECT_FILTER => Filter::DESCRIPTORS,
        EFFECT_ECHO => Echo::DESCRIPTORS,
        EFFECT_REVERB => Reverb::DESCRIPTORS,
        EFFECT_GATE => Gate::DESCRIPTORS,
        _ => &[],
    }
}

/// Build the param defaults table for an effect, populating slot 0..N
/// with `descriptor.default` and the remainder with 0.0.
pub fn default_params(effect_id: EffectId) -> EffectParams {
    let mut p = [0.0_f32; MAX_PARAMS];
    let descs = descriptors(effect_id);
    for (i, d) in descs.iter().enumerate() {
        if i < MAX_PARAMS {
            p[i] = d.default;
        }
    }
    p
}

/// A pre-allocated bank holding all 4 effect instances. Each slot in
/// `mixer.rs` owns one of these; the active `effect_id` selects which
/// gets `process()`'d. Switching slot assignment just `reset()`s the
/// new target — no allocation. ADR-006 review (Codex): pre-alloc beats
/// pointer-swap because it sidesteps any free/init on the audio thread.
pub struct FxBank {
    pub filter: Filter,
    pub echo: Echo,
    pub reverb: Reverb,
    pub gate: Gate,
    /// 0 = empty, else one of `EFFECT_FILTER..EFFECT_GATE`.
    pub effect_id: EffectId,
    /// 0..MAX_PARAMS f32 params; effect-defined ordering.
    pub params: EffectParams,
    /// 0..1 wet/dry blend. 1.0 = full wet.
    pub wet_dry: f32,
    pub enabled: bool,
    /// Optional per-slot LFO modulating one chosen param. `None` =
    /// static params (legacy behaviour). Configured / cleared via
    /// `AudioCommandKind::EffectLfoSet` / `EffectLfoClear`. POD —
    /// `LfoConfig: Copy` so this stays alloc-free.
    pub lfo: Option<Lfo>,
    /// Snapshot of the SharedClock so the LFO can re-read live BPM +
    /// frame each `process()` call. Cloning a SharedClock is a single
    /// `Arc` bump — alloc-free; not held inside the `Option<Lfo>` so
    /// we don't re-clone the Arc on every event.
    clock: SharedClock,
}

impl FxBank {
    /// Pre-allocate every effect for one slot. `clock` is shared with
    /// the audio thread for beat-synced effects (Gate).
    pub fn new(sample_rate: u32, clock: SharedClock, master_bpm: f32) -> Self {
        Self {
            filter: Filter::new(),
            echo: Echo::new(sample_rate),
            reverb: Reverb::new(sample_rate),
            gate: Gate::new(clock.clone(), sample_rate, master_bpm),
            effect_id: EFFECT_NONE,
            params: [0.0; MAX_PARAMS],
            wet_dry: 0.5,
            enabled: false,
            lfo: None,
            clock,
        }
    }

    /// Attach (or replace) the slot's modulation LFO. Pure metadata —
    /// no audio-thread state to reset because the LFO is a pure-fn of
    /// (frame, bpm, sr).
    #[inline]
    pub fn set_lfo(&mut self, config: LfoConfig) {
        self.lfo = Some(Lfo::new(config));
    }

    /// Detach any attached LFO. Subsequent `process()` calls run with
    /// the slot's static params.
    #[inline]
    pub fn clear_lfo(&mut self) {
        self.lfo = None;
    }

    /// Re-assign this slot to a different effect id. `reset()` the
    /// target so its internal state doesn't bleed from a previous use.
    #[inline]
    pub fn assign(&mut self, effect_id: EffectId) {
        self.effect_id = effect_id;
        self.params = default_params(effect_id);
        self.wet_dry = 0.5;
        self.enabled = true;
        // Re-assigning the slot to a different effect implicitly clears
        // any prior LFO: the old `target_param` index almost certainly
        // means something different under the new effect's descriptor
        // table, so silently re-applying it is a footgun.
        self.lfo = None;
        match effect_id {
            EFFECT_FILTER => self.filter.reset(),
            EFFECT_ECHO => self.echo.reset(),
            EFFECT_REVERB => self.reverb.reset(),
            EFFECT_GATE => self.gate.reset(),
            _ => {}
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.effect_id = EFFECT_NONE;
        self.enabled = false;
        self.lfo = None;
    }

    /// Update one param by descriptor index. Out-of-range index is
    /// ignored (defensive; the translator should never emit one).
    #[inline]
    pub fn set_param(&mut self, idx: u8, value: f32) {
        let i = idx as usize;
        if i < MAX_PARAMS {
            // Clamp using the descriptor range (control-thread had
            // already resolved the param; we still defensive-clamp).
            let descs = descriptors(self.effect_id);
            let clamped = descs.get(i).map(|d| d.clamp(value)).unwrap_or(value);
            self.params[i] = clamped;
        }
    }

    /// **Audio-thread side**. Process `buf` (interleaved stereo) in
    /// place. No-op when slot is empty / disabled / fully dry.
    ///
    /// If `self.lfo` is `Some`, the slot's static `params` are modulated
    /// per [`apply_lfo_to_params`] before dispatching to the effect's
    /// `process()`. The modulation is computed once per buffer (at the
    /// buffer's start frame) — sufficient for sub-audio-rate LFOs at
    /// typical render-block sizes (256-2048 frames) without paying the
    /// CPU for per-sample modulation. POD copy of the params table
    /// stays on the stack — no allocation.
    #[inline]
    pub fn process(&mut self, buf: &mut [f32], sample_rate: u32) {
        if !self.enabled || self.effect_id == EFFECT_NONE || self.wet_dry <= 0.0 {
            return;
        }
        let wet = self.wet_dry.clamp(0.0, 1.0);
        // Effective params: either the static table or a per-buffer
        // LFO-modulated snapshot.
        let params = match &self.lfo {
            None => self.params,
            Some(lfo) => {
                let bpm = self.clock.master_bpm();
                let frame = self.clock.frame();
                apply_lfo_to_params(&self.params, lfo, self.effect_id, frame, sample_rate, bpm)
            }
        };
        match self.effect_id {
            EFFECT_FILTER => self.filter.process(buf, &params, wet, sample_rate),
            EFFECT_ECHO => self.echo.process(buf, &params, wet, sample_rate),
            EFFECT_REVERB => self.reverb.process(buf, &params, wet, sample_rate),
            EFFECT_GATE => self.gate.process(buf, &params, wet, sample_rate),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank() -> FxBank {
        FxBank::new(48_000, SharedClock::new(), 120.0)
    }

    #[test]
    fn descriptors_resolve_param_names() {
        assert_eq!(resolve_param(EFFECT_FILTER, "cutoff_hz"), Some(0));
        assert_eq!(resolve_param(EFFECT_FILTER, "resonance"), Some(1));
        assert_eq!(resolve_param(EFFECT_FILTER, "mode"), Some(2));
        assert_eq!(resolve_param(EFFECT_FILTER, "nonsense"), None);
        assert_eq!(resolve_param(EFFECT_ECHO, "time_ms"), Some(0));
        assert_eq!(resolve_param(EFFECT_ECHO, "feedback"), Some(1));
        assert_eq!(resolve_param(EFFECT_ECHO, "tone"), Some(2));
        assert_eq!(resolve_param(EFFECT_REVERB, "room_size"), Some(0));
        assert_eq!(resolve_param(EFFECT_GATE, "period_div"), Some(0));
        assert_eq!(resolve_param(99, "anything"), None);
    }

    #[test]
    fn default_params_uses_descriptor_defaults() {
        let p = default_params(EFFECT_FILTER);
        // cutoff_hz default 500
        assert!((p[0] - 500.0).abs() < 1e-6);
        // resonance default 0.3
        assert!((p[1] - 0.3).abs() < 1e-6);
        // mode default 0 (LP)
        assert!((p[2] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn disabled_slot_is_passthrough() {
        let mut b = bank();
        b.assign(EFFECT_FILTER);
        b.enabled = false;
        let mut buf = [0.5_f32; 64];
        b.process(&mut buf, 48_000);
        // Disabled → buffer untouched.
        assert!(buf.iter().all(|s| (*s - 0.5).abs() < 1e-9));
    }

    #[test]
    fn empty_slot_is_passthrough() {
        let mut b = bank();
        // effect_id == EFFECT_NONE
        let mut buf = [0.7_f32; 64];
        b.process(&mut buf, 48_000);
        assert!(buf.iter().all(|s| (*s - 0.7).abs() < 1e-9));
    }

    #[test]
    fn assign_resets_effect_state() {
        let mut b = bank();
        b.assign(EFFECT_ECHO);
        // Push something into the echo's delay line.
        let mut buf = [1.0_f32; 64];
        b.process(&mut buf, 48_000);
        b.assign(EFFECT_ECHO); // re-assign → reset()
                               // After reset the delay line is silent: feeding zeros must
                               // produce wet=zero output.
        let mut buf2 = [0.0_f32; 64];
        b.process(&mut buf2, 48_000);
        assert!(buf2.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn lfo_attached_to_filter_slot_modulates_cutoff() {
        // Verify the FxBank-level wiring: attaching an LFO to a Filter
        // slot causes the params seen by the inner effect to vary with
        // the SharedClock frame. We assert on the *params snapshot* the
        // bank computes (cheaper + more robust than measuring filter
        // output energy across biquad transients).
        let clock = SharedClock::with_bpm(240.0);
        let mut bank = FxBank::new(48_000, clock.clone(), 240.0);
        bank.assign(EFFECT_FILTER);
        bank.params[0] = 1000.0; // cutoff_hz
        bank.params[1] = 0.0; // resonance
        bank.params[2] = 0.0; // LP
        bank.set_lfo(LfoConfig::new(Shape::Sine, RateDiv::Bar, 1.0, 0));

        // At frame 0 the sine LFO is 0 → cutoff unchanged.
        {
            let lfo = bank.lfo.as_ref().expect("lfo set");
            let p = apply_lfo_to_params(
                &bank.params,
                lfo,
                bank.effect_id,
                clock.frame(),
                48_000,
                clock.master_bpm(),
            );
            assert!(
                (p[0] - 1000.0).abs() < 1e-3,
                "LFO @ phase 0 should leave cutoff at 1000, got {}",
                p[0]
            );
        }
        // Advance to 1/4 LFO cycle (12_000 frames @ Bar / 240 BPM / 48 kHz).
        clock.advance(12_000);
        {
            let lfo = bank.lfo.as_ref().expect("lfo set");
            let p = apply_lfo_to_params(
                &bank.params,
                lfo,
                bank.effect_id,
                clock.frame(),
                48_000,
                clock.master_bpm(),
            );
            // +1 sine × depth=1 × 2 octaves → cutoff × 4 = 4000 Hz.
            assert!(
                (p[0] - 4000.0).abs() < 5.0,
                "LFO @ phase 1/4 should swing cutoff to ~4000, got {}",
                p[0]
            );
        }
        // End-to-end: confirm `process()` still runs without panic /
        // NaN at the modulated state.
        let mut buf = vec![0.5_f32; 2048];
        bank.process(&mut buf, 48_000);
        assert!(buf.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn assigning_new_effect_clears_lfo() {
        let mut b = bank();
        b.assign(EFFECT_FILTER);
        b.set_lfo(LfoConfig::new(Shape::Sine, RateDiv::Beat, 1.0, 0));
        assert!(b.lfo.is_some());
        // Re-assign to a different effect → LFO cleared. Stale
        // target_param under a new descriptor table would be a footgun.
        b.assign(EFFECT_ECHO);
        assert!(b.lfo.is_none(), "assigning a new effect must clear the LFO");
    }

    #[test]
    fn clear_slot_clears_lfo() {
        let mut b = bank();
        b.assign(EFFECT_FILTER);
        b.set_lfo(LfoConfig::new(Shape::Sine, RateDiv::Beat, 1.0, 0));
        b.clear();
        assert!(b.lfo.is_none(), "clearing a slot must clear the LFO");
    }

    #[test]
    fn assert_no_alloc_process_with_lfo() {
        let clock = SharedClock::with_bpm(120.0);
        let mut b = FxBank::new(48_000, clock, 120.0);
        b.assign(EFFECT_FILTER);
        b.set_lfo(LfoConfig::new(Shape::Sine, RateDiv::Quarter, 0.6, 0));
        let mut buf = [0.1_f32; 2048];
        assert_no_alloc::assert_no_alloc(|| {
            b.process(&mut buf, 48_000);
        });
    }

    #[test]
    fn assert_no_alloc_full_chain() {
        let mut filter_bank = bank();
        filter_bank.assign(EFFECT_FILTER);
        let mut echo_bank = bank();
        echo_bank.assign(EFFECT_ECHO);
        let mut reverb_bank = bank();
        reverb_bank.assign(EFFECT_REVERB);
        let mut gate_bank = bank();
        gate_bank.assign(EFFECT_GATE);

        let mut buf = [0.1_f32; 1024];
        assert_no_alloc::assert_no_alloc(|| {
            filter_bank.process(&mut buf, 48_000);
            echo_bank.process(&mut buf, 48_000);
            reverb_bank.process(&mut buf, 48_000);
            gate_bank.process(&mut buf, 48_000);
        });
    }

    /// Print measured per-1024-frame worst-case effect latency. This is
    /// a perf test, not a correctness one — output is captured by
    /// `cargo test -- --nocapture` for the PR description.
    #[test]
    fn perf_worst_case_1024_frame_latency() {
        use std::time::Instant;
        let mut banks = [
            (EFFECT_FILTER, bank()),
            (EFFECT_ECHO, bank()),
            (EFFECT_REVERB, bank()),
            (EFFECT_GATE, bank()),
        ];
        for (id, b) in banks.iter_mut() {
            b.assign(*id);
        }
        let mut buf = [0.1_f32; 1024];
        // Warm-up
        for _ in 0..16 {
            for (_, b) in banks.iter_mut() {
                b.process(&mut buf, 48_000);
            }
        }
        let mut worst_ns: u128 = 0;
        for _ in 0..1000 {
            for (_, b) in banks.iter_mut() {
                // measure per-effect worst case
                let t = Instant::now();
                b.process(&mut buf, 48_000);
                let ns = t.elapsed().as_nanos();
                if ns > worst_ns {
                    worst_ns = ns;
                }
            }
        }
        // 1024 stereo frames @ 48kHz = ~10.7ms budget. Each effect
        // alone should be well under 500µs even on a slow runner.
        eprintln!(
            "effect_chain_worst_case_1024frames_ns={worst_ns} (~{:.2}µs)",
            worst_ns as f64 / 1000.0
        );
        assert!(
            worst_ns < 5_000_000,
            "effect process took {worst_ns} ns for 1024 frames — over 5 ms budget"
        );
    }
}
