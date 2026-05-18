//! Audio-thread performance metrics — CPU usage, render latency, underrun
//! counts.
//!
//! Live DJs need to know if the engine is keeping up. This module
//! aggregates four signals into a single [`PerfSnapshot`] that the bridge
//! stamps onto every `engine.state_changed` notification:
//!
//! * **CPU%** — average render time ÷ callback period × 100. Sub-50% is
//!   green; 50..80% is yellow; ≥80% is red — that means the audio thread
//!   is spending most of its budget rendering, and any extra effect /
//!   stem / pitch-tempo work risks an xrun.
//! * **Render p99 (µs)** — rolling-window max render time. Captures the
//!   spiky worst-case (rubato refill, decode catch-up) without being
//!   washed out by the long-run average.
//! * **Audio underruns** — cpal-side xrun count (host underrun
//!   notifications via the `err_fn`). Not surfaced today via this struct,
//!   but the field is here for symmetry; xruns roll up into the same
//!   `state_changed` payload as `audio_xrun_count` on `BridgeMetrics`.
//! * **Decode underruns** — pulled from the [`DecodeService`] — number of
//!   times the audio thread asked for samples that the decoder ring
//!   couldn't supply (zero-padded silence). A non-zero value indicates
//!   the decoder is falling behind I/O / scheduling jitter.
//! * **Dropped frames (recorder)** — `MasterRecorderSink::dropped_frames`,
//!   the number of stereo frames the recorder couldn't accept because
//!   its ring overflowed. Surfaces a slow-disk recording bottleneck.
//!
//! # Hard rules (ADR-004)
//!
//! All counters are `AtomicU64` — the audio thread updates them with
//! `fetch_add` / atomic stores. No allocation, no mutex, no blocking.
//! Snapshot reads happen off the audio thread (bridge), so a snapshot
//! load is allowed to do per-field `load(Relaxed)` plus arithmetic.
//!
//! # Why not a histogram
//!
//! A real p99 needs reservoir sampling or HDR. Both allocate or use a
//! `Mutex` — not allowed on the audio thread. We approximate with a
//! "max-in-window" reading instead: the audio thread bumps
//! `max_render_ns_window` whenever the current render exceeds it. The
//! bridge thread reads the current window max **and atomically resets
//! it** every snapshot, so each snapshot reports the peak since the last
//! poll. At 5 Hz state_changed cadence that's a 200 ms window — exactly
//! the size a DJ would visually notice anyway.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Default cpal callback period when the real value isn't yet known
/// (used by tests). Production main() passes the actual buffer size /
/// sample rate.
const DEFAULT_CALLBACK_PERIOD_NS: u64 = 10_667_000; // 512 frames @ 48 kHz

/// Live audio-thread metrics. Cloneable handle (`Arc` inside) so the
/// audio thread + bridge thread can both reach the counters cheaply.
///
/// `PerfMetrics::default()` is the test path — defaults to a 10.67 ms
/// callback period (512 frames @ 48 kHz). Production: build via
/// [`PerfMetrics::with_callback_period`].
#[derive(Clone, Debug)]
pub struct PerfMetrics {
    inner: Arc<PerfInner>,
}

/// The atomic counters live behind an `Arc` so the audio thread holds
/// one clone (no atomics-copy on every render — just a pointer chase) and
/// the bridge thread holds another. All fields are `AtomicU64`; updates
/// from the audio thread use `Relaxed` ordering (single-writer per
/// callback) and the bridge's snapshot loads use `Relaxed` too — we don't
/// need cross-thread happens-before, just monotonic visibility.
#[derive(Debug)]
struct PerfInner {
    /// Total number of `render()` calls observed by the audio thread.
    /// Used as the divisor in the running-average CPU% calculation.
    render_count: AtomicU64,
    /// Cumulative wall-clock ns spent inside `render()` across all
    /// observed calls. Divided by `render_count` for the running avg.
    total_render_ns: AtomicU64,
    /// Max single-render duration (ns) observed in the current snapshot
    /// window. The audio thread bumps this via `fetch_max`; the bridge
    /// thread atomically swaps it back to 0 on each snapshot read.
    max_render_ns_window: AtomicU64,
    /// cpal-side audio-callback underrun count. The cpal `err_fn` is
    /// invoked off the realtime path on stream errors; we map every
    /// error to a single increment here. Surfaces as `underrun_count`.
    underrun_count_audio: AtomicU64,
    /// Stereo frames the recorder couldn't accept because its ring
    /// overflowed (slow disk, kernel page-cache pressure). Mirror of
    /// `MasterRecorderSink::dropped_frames`, but pre-populated into the
    /// snapshot so the wire payload doesn't have to plumb the recorder
    /// handle into the bridge.
    dropped_frames_recorder: AtomicU64,
    /// Audio-thread reads-against-empty in the decoder ring. Sampled
    /// from `DecodeService::underrun_count()` each snapshot.
    decode_underruns: AtomicU64,
    /// Audio callback period in ns. Constant for the lifetime of the
    /// metric (until the audio device is re-opened). 48 kHz × 512
    /// frame buffer ≈ 10.67 ms = 10_667_000 ns. Used as the CPU%
    /// denominator: cpu% = avg_render_ns / callback_period_ns × 100.
    callback_period_ns: AtomicU64,
}

impl Default for PerfMetrics {
    fn default() -> Self {
        Self::with_callback_period(DEFAULT_CALLBACK_PERIOD_NS)
    }
}

impl PerfMetrics {
    /// Build a metrics handle pinned to the given cpal callback period
    /// (ns). Production: derive `period_ns` from the device sample rate
    /// + buffer-size hint:
    ///
    /// ```text
    ///   period_ns = frame_count × 1_000_000_000 / sample_rate
    /// ```
    pub fn with_callback_period(period_ns: u64) -> Self {
        Self {
            inner: Arc::new(PerfInner {
                render_count: AtomicU64::new(0),
                total_render_ns: AtomicU64::new(0),
                max_render_ns_window: AtomicU64::new(0),
                underrun_count_audio: AtomicU64::new(0),
                dropped_frames_recorder: AtomicU64::new(0),
                decode_underruns: AtomicU64::new(0),
                callback_period_ns: AtomicU64::new(period_ns.max(1)),
            }),
        }
    }

    /// Derive callback period from buffer size + sample rate. Production
    /// `main.rs` calls this once `AudioStreamHandle` is built. The
    /// result is clamped to ≥ 1 ns so the CPU% divisor never zeros.
    pub fn callback_period_from(frame_count: u32, sample_rate: u32) -> u64 {
        if sample_rate == 0 || frame_count == 0 {
            return DEFAULT_CALLBACK_PERIOD_NS;
        }
        // u128 to avoid the intermediate `frame_count × 1e9` overflow.
        let ns = (frame_count as u128).saturating_mul(1_000_000_000) / (sample_rate as u128);
        (ns as u64).max(1)
    }

    /// Update the callback period (e.g. when the cpal stream is
    /// re-opened with a new buffer size). Bridge-side; not the audio
    /// thread.
    pub fn set_callback_period(&self, period_ns: u64) {
        self.inner
            .callback_period_ns
            .store(period_ns.max(1), Ordering::Relaxed);
    }

    /// Audio-thread side: record one completed render. Receives the
    /// observed wall-clock duration in ns. Alloc-free, lock-free, two
    /// `fetch_add`s + one `fetch_max` — well under a microsecond on
    /// every platform we ship to.
    ///
    /// `render_ns` should be measured around the inner `mixer.render`
    /// call; do NOT include the cpal `data` write-back (that's outside
    /// the engine's control). See `io.rs` for the integration point.
    #[inline]
    pub fn record_render_ns(&self, render_ns: u64) {
        self.inner.render_count.fetch_add(1, Ordering::Relaxed);
        self.inner
            .total_render_ns
            .fetch_add(render_ns, Ordering::Relaxed);
        // Window-max via `fetch_max`. The bridge resets it to 0 each
        // snapshot so the field acts as a sliding-window peak.
        let _ = self
            .inner
            .max_render_ns_window
            .fetch_max(render_ns, Ordering::Relaxed);
    }

    /// Audio-thread (or cpal err_fn) side: bump the audio-callback
    /// underrun count. Single atomic increment.
    #[inline]
    pub fn record_audio_underrun(&self) {
        self.inner
            .underrun_count_audio
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Bridge-side helper: stamp the recorder's dropped-frame count
    /// into the snapshot store before the next `snapshot()` read.
    /// Called once per `state_changed` from the sampler that already
    /// reads `MasterRecorderSink::dropped_frames`.
    pub fn set_dropped_frames_recorder(&self, frames: u64) {
        self.inner
            .dropped_frames_recorder
            .store(frames, Ordering::Relaxed);
    }

    /// Bridge-side helper: stamp the decoder ring's underrun count.
    /// Called once per `state_changed` from the sampler that reads
    /// `DecodeService::underrun_count()`.
    pub fn set_decode_underruns(&self, count: u64) {
        self.inner.decode_underruns.store(count, Ordering::Relaxed);
    }

    /// Take a snapshot of the current metrics. Atomically resets
    /// `max_render_ns_window` to zero so the next snapshot reports the
    /// peak since this call — implementing the sliding-window p99
    /// approximation described in the module docs.
    pub fn snapshot(&self) -> PerfSnapshot {
        let count = self.inner.render_count.load(Ordering::Relaxed);
        let total_ns = self.inner.total_render_ns.load(Ordering::Relaxed);
        let max_ns = self.inner.max_render_ns_window.swap(0, Ordering::Relaxed);
        let underrun_count = self.inner.underrun_count_audio.load(Ordering::Relaxed);
        let dropped_frames = self.inner.dropped_frames_recorder.load(Ordering::Relaxed);
        let decode_underruns = self.inner.decode_underruns.load(Ordering::Relaxed);
        let period_ns = self.inner.callback_period_ns.load(Ordering::Relaxed).max(1);

        let avg_render_ns = total_ns.checked_div(count).unwrap_or(0);
        let cpu_percent = if period_ns == 0 {
            0.0
        } else {
            (avg_render_ns as f64 / period_ns as f64) * 100.0
        };
        // Cap to a sane 0..200 window so a single rogue render (say a
        // first-call rubato prime) can't paint the gauge with a value
        // that drowns out the legend.
        let cpu_percent = cpu_percent.clamp(0.0, 200.0) as f32;

        PerfSnapshot {
            cpu_percent,
            render_p99_us: (max_ns / 1_000) as u32,
            avg_render_us: (avg_render_ns / 1_000) as u32,
            underrun_count,
            dropped_frames,
            decode_underruns,
            callback_period_us: (period_ns / 1_000) as u32,
            render_count: count,
        }
    }
}

/// Wire-shaped perf payload — rides on every `engine.state_changed`
/// notification next to `state`. Pure data; no methods. Mirrored
/// 1-for-1 by the UI store (see `ui/src/store/perf.ts`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct PerfSnapshot {
    /// Average render time as a percent of the callback period.
    /// 50% green / 50..80% yellow / ≥80% red — UI applies the colors.
    pub cpu_percent: f32,
    /// Per-snapshot peak render time in µs (sliding window, reset on
    /// each `snapshot()` call). Approximates p99 without needing a
    /// histogram on the audio thread.
    pub render_p99_us: u32,
    /// Long-run average render time in µs. Survives across snapshots
    /// (we don't reset `total_render_ns`).
    pub avg_render_us: u32,
    /// cpal stream-error count (host-reported underruns).
    pub underrun_count: u64,
    /// Recorder ring overflows in stereo frames.
    pub dropped_frames: u64,
    /// Decoder ring underruns (samples the audio thread asked for that
    /// the decoder couldn't supply — zero-padded with silence).
    pub decode_underruns: u64,
    /// Audio callback period (µs). Bundled into the snapshot so the UI
    /// can render "render_p99 / callback_period" as a sanity ratio.
    pub callback_period_us: u32,
    /// Total render() calls since boot. Useful for "we recorded N
    /// samples" tooltips and for the rolling-history graph stride.
    pub render_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_period_derivation_matches_48k_512buf() {
        // 512 frames @ 48 kHz = 10_666_666.67 ns → floor to 10_666_666.
        let p = PerfMetrics::callback_period_from(512, 48_000);
        // Allow ±1 ns rounding from the integer division.
        assert!(
            (10_666_665..=10_666_667).contains(&p),
            "got {p}, expected ~10666666"
        );
    }

    #[test]
    fn callback_period_guards_against_zero_inputs() {
        // Zero sample rate or zero frames must fall back to the
        // documented default, not divide-by-zero.
        assert_eq!(
            PerfMetrics::callback_period_from(0, 48_000),
            DEFAULT_CALLBACK_PERIOD_NS
        );
        assert_eq!(
            PerfMetrics::callback_period_from(512, 0),
            DEFAULT_CALLBACK_PERIOD_NS
        );
    }

    #[test]
    fn record_render_accumulates_counters_correctly() {
        // Record 5 renders of 1ms each — count + total + max should
        // all line up with the expected sums.
        let m = PerfMetrics::with_callback_period(10_000_000); // 10 ms
        for _ in 0..5 {
            m.record_render_ns(1_000_000); // 1 ms
        }
        let snap = m.snapshot();
        assert_eq!(snap.render_count, 5);
        assert_eq!(snap.avg_render_us, 1_000); // 1 ms = 1000 µs
                                               // 1ms render / 10ms period = 10% CPU.
        assert!(
            (snap.cpu_percent - 10.0).abs() < 1e-3,
            "expected ~10% CPU, got {}",
            snap.cpu_percent
        );
    }

    #[test]
    fn cpu_percent_calculation_matches_avg_ratio() {
        // Pin a 5 ms render in a 10 ms callback → 50% CPU.
        let m = PerfMetrics::with_callback_period(10_000_000);
        m.record_render_ns(5_000_000);
        let snap = m.snapshot();
        assert!(
            (snap.cpu_percent - 50.0).abs() < 1e-3,
            "expected 50% CPU, got {}",
            snap.cpu_percent
        );
    }

    #[test]
    fn snapshot_resets_window_max_for_next_window() {
        // The sliding-window peak must atomically reset on snapshot —
        // otherwise a single spike would pin the meter at the peak
        // forever.
        let m = PerfMetrics::with_callback_period(10_000_000);
        m.record_render_ns(3_000_000); // 3 ms
        let s1 = m.snapshot();
        assert_eq!(s1.render_p99_us, 3_000);
        // Next window: smaller render. Peak should be the new render,
        // NOT the prior 3 ms.
        m.record_render_ns(500_000); // 0.5 ms
        let s2 = m.snapshot();
        assert_eq!(s2.render_p99_us, 500);
    }

    #[test]
    fn record_audio_underrun_increments_counter() {
        // Distinct counter from decode underruns — cpal-side xruns.
        let m = PerfMetrics::default();
        m.record_audio_underrun();
        m.record_audio_underrun();
        m.record_audio_underrun();
        let snap = m.snapshot();
        assert_eq!(snap.underrun_count, 3);
    }

    #[test]
    fn snapshot_returns_zero_avg_when_no_renders_observed() {
        // Defensive: empty PerfMetrics must not divide by zero or
        // surface a NaN cpu_percent (would break the UI gauge color
        // selector).
        let m = PerfMetrics::default();
        let snap = m.snapshot();
        assert_eq!(snap.render_count, 0);
        assert_eq!(snap.avg_render_us, 0);
        assert!(snap.cpu_percent.is_finite() && snap.cpu_percent == 0.0);
    }

    #[test]
    fn set_dropped_frames_and_decode_underruns_are_reflected_in_snapshot() {
        // The bridge-side setters feed the snapshot — verify both
        // round-trip.
        let m = PerfMetrics::default();
        m.set_dropped_frames_recorder(42);
        m.set_decode_underruns(7);
        let snap = m.snapshot();
        assert_eq!(snap.dropped_frames, 42);
        assert_eq!(snap.decode_underruns, 7);
    }

    #[test]
    fn record_render_is_alloc_free() {
        // ADR-004: the audio thread cannot allocate. `record_render_ns`
        // is on the hot path — gate it with assert_no_alloc.
        let m = PerfMetrics::default();
        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..1000 {
                m.record_render_ns(1_000 + i);
            }
        });
    }

    #[test]
    fn cpu_percent_clamps_to_sane_window() {
        // A 100ms render in a 10ms callback would mathematically yield
        // 1000% CPU. Clamp at 200% so the UI gauge never gets a value
        // that explodes its color logic.
        let m = PerfMetrics::with_callback_period(10_000_000);
        m.record_render_ns(100_000_000);
        let snap = m.snapshot();
        assert!(
            snap.cpu_percent <= 200.0,
            "expected clamp at 200%, got {}",
            snap.cpu_percent
        );
    }

    #[test]
    fn snapshot_serialises_to_stable_json_shape() {
        // Wire contract — the UI mirror depends on these field names.
        let m = PerfMetrics::with_callback_period(10_000_000);
        m.record_render_ns(2_000_000);
        m.set_dropped_frames_recorder(5);
        m.set_decode_underruns(3);
        let snap = m.snapshot();
        let json = serde_json::to_value(snap).unwrap();
        for field in [
            "cpu_percent",
            "render_p99_us",
            "avg_render_us",
            "underrun_count",
            "dropped_frames",
            "decode_underruns",
            "callback_period_us",
            "render_count",
        ] {
            assert!(json.get(field).is_some(), "missing field: {field}");
        }
        assert_eq!(json["dropped_frames"].as_u64(), Some(5));
        assert_eq!(json["decode_underruns"].as_u64(), Some(3));
    }
}
