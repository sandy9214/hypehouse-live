//! WSOLA (Waveform-Similarity OverLap-Add) time-stretch — stage 2 of
//! the pitch + tempo cascade.
//!
//! # Why
//!
//! PR #43 introduced [`crate::audio::pitch_tempo::PitchTempo`], a
//! two-stage cascade where stage 1 resamples for pitch shift and
//! stage 2 resamples to undo the tempo side-effect of stage 1.
//!
//! Cascading two sample-rate converters (rubato `FastFixedIn` × 2) is
//! mathematically a *single* resample by the product ratio — so when
//! both knobs are non-default the user hears the SAME thing as a
//! single-stage resample. At ±50% tempo this means the pitch shifts
//! along with tempo: pitch and tempo are NOT truly orthogonal.
//!
//! True orthogonality needs a time-domain time-stretch that does
//! **not** change pitch. WSOLA is the textbook real-time-friendly
//! algorithm:
//!
//! 1. Slide a windowed input frame across the input stream at output
//!    hop intervals.
//! 2. For each output frame, search a small window in the input for
//!    the segment that BEST matches the previous output frame's tail
//!    (cross-correlation, here implemented as cheap SAD = sum of
//!    absolute differences).
//! 3. Window that segment with a Hann taper and overlap-add into the
//!    output buffer.
//!
//! The output is the input stretched in time **without** pitch change:
//! frequencies are preserved because we only re-arrange existing
//! waveform segments — we never resample.
//!
//! # Algorithm (concrete)
//!
//! Per `process()` call:
//!
//! * Push the caller-supplied mono input into the input ring buffer
//!   (`in_ring`).
//! * While we have at least one full search window's worth of input
//!   ahead of the read cursor:
//!     1. Pick the candidate start = read cursor.
//!     2. SAD-search the next `search_range` samples for the segment
//!        whose first `overlap_len` windowed samples best match the
//!        last emitted output frame's tail (the "natural continuation"
//!        criterion — gives WSOLA its pitch-preservation property).
//!     3. Window that `window_size`-sample segment with the
//!        pre-computed Hann window and overlap-add into the running
//!        OLA accumulator.
//!     4. Cache the last `overlap_len` samples of the windowed
//!        segment as the matcher target for the next iteration.
//!     5. Advance the read cursor by `hop_size_in = hop_size_out /
//!        ratio` (synthesis hop is fixed; analysis hop varies).
//!     6. Emit `hop_size_out` samples to the caller (or to a pending
//!        queue if the caller's slice is full).
//!     7. Slide the OLA accumulator down by `hop_size_out` so the
//!        next iteration's overlap region is at index 0.
//!
//! `ratio < 1` → analysis hop < synthesis hop → slows down.
//! `ratio > 1` → analysis hop > synthesis hop → speeds up.
//! `ratio = 1` → analysis hop = synthesis hop → near-passthrough
//! (modulo the windowing overlap).
//!
//! # Audio-thread safety (ADR-004)
//!
//! All state — input ring, output overlap-add accumulator, pending
//! emit queue, last-frame cache, pre-computed Hann window — is
//! allocated in [`Wsola::new`] and re-used across [`Wsola::process`]
//! calls. The hot path performs ONLY array reads + writes +
//! arithmetic. The `wsola_alloc_free` test enforces this.
//!
//! No `unsafe` is used. SAD is an O(overlap × search_range) inner
//! loop, ~512 × 256 = 131k abs-diffs per output hop, with an early-out
//! when the running score exceeds the best-so-far. On an M-class
//! laptop release build this is comfortably under the audio-thread
//! budget at the default parameters (see
//! `wsola_process_latency_under_budget`).
//!
//! # Mono / stereo
//!
//! This module operates on mono channel slices. The caller
//! ([`crate::audio::pitch_tempo`]) holds two `Wsola` instances per
//! deck (L + R) and feeds them in parallel. Independent matcher state
//! per channel preserves stereo phase relationships.

/// Default analysis/synthesis window length (samples). 1024 samples at
/// 48 kHz = 21.3 ms — comfortably above the ~30 Hz floor below which
/// WSOLA introduces flutter, comfortably below the ~50 ms ceiling
/// above which transients smear.
pub const DEFAULT_WINDOW_SIZE: usize = 1024;

/// Default output synthesis hop (samples). 512 = WINDOW_SIZE / 2 →
/// 50% overlap, the textbook WSOLA setting (Hanning-windowed 50%
/// overlap sums to unity-gain).
pub const DEFAULT_HOP_OUT: usize = 512;

/// Default SAD search range (samples). 256 covers pitch-period drift
/// down to ~94 Hz at 48 kHz (≈ a low bass fundamental) — wider than
/// needed for typical pop / electronic material but cheap because the
/// SAD loop has an early-out branch.
pub const DEFAULT_SEARCH_RANGE: usize = 256;

/// Input ring buffer length. Sized to cover several worst-case
/// audio-callback chunks at MIN_TEMPO_RATIO (= 0.5, so hop_in = 2 ×
/// hop_out = 1024 samples per iteration) plus a healthy safety margin
/// so the audio thread can stall briefly without underflowing.
pub const RING_LEN: usize = (DEFAULT_WINDOW_SIZE + DEFAULT_SEARCH_RANGE + DEFAULT_HOP_OUT * 4) * 2;

/// Audio-thread-safe WSOLA processor.
///
/// One instance per audio channel. Caller drives it by:
/// 1. [`Wsola::set_ratio`] (O(1), audio-thread safe).
/// 2. [`Wsola::process`] feed `input`, receive frames into `output`.
///
/// All buffers are pre-allocated in [`Wsola::new`]. No heap traffic
/// after construction; see module-level docs for the audio-thread
/// safety contract.
pub struct Wsola {
    /// Window length (samples). Same value used as analysis + synthesis
    /// window — standard WSOLA.
    window_size: usize,
    /// Synthesis (output) hop length. Fixed regardless of ratio.
    hop_size_out: usize,
    /// Analysis (input) hop length. Updated by [`Wsola::set_ratio`].
    hop_size_in: usize,
    /// SAD search half-range — number of input samples to scan ahead
    /// of the analysis cursor when looking for the best matcher.
    search_range: usize,
    /// Overlap length = window_size - hop_size_out (samples of
    /// crossfade between consecutive synthesis frames).
    overlap_len: usize,
    /// Pre-computed Hann window of length `window_size`. Built once
    /// in `new()`; same coefficients reused on every synthesis frame.
    hann: Vec<f32>,
    /// Input ring buffer (linear; we never wrap, we compact in place
    /// once the consumed prefix grows past half the buffer). Sized to
    /// `RING_LEN`.
    in_ring: Vec<f32>,
    /// Write cursor inside `in_ring` — index of the next slot to fill.
    in_ring_w: usize,
    /// Read cursor inside `in_ring` — analysis start position for the
    /// next synthesis frame.
    in_ring_r: usize,
    /// Overlap-add accumulator. Length = `window_size`. Each
    /// iteration:
    /// * `[0..overlap_len]` starts the frame pre-filled with the
    ///   previous frame's tail (already windowed); the new windowed
    ///   frame is overlap-added on top.
    /// * `[overlap_len..window_size]` is overwritten with the new
    ///   windowed mid + tail.
    /// * After the iteration `[0..hop_size_out]` is "finished" output;
    ///   `[hop_size_out..window_size]` is slid down to become the next
    ///   iteration's overlap region.
    oa_buf: Vec<f32>,
    /// True once we've synthesised ≥ 1 frame — controls whether
    /// `oa_buf[0..overlap_len]` is pre-primed with overlap (it is
    /// only after the first frame).
    primed: bool,
    /// Pending emit queue — finished samples we couldn't hand to the
    /// caller because their `output` slice ran out of room. Drained
    /// on the next `process()` call before any new synthesis. Length
    /// = `hop_size_out` is sufficient because we stop synthesising
    /// when the queue is non-empty; at most one hop's worth queues
    /// up at a time.
    pending_emit: Vec<f32>,
    /// Number of valid samples in `pending_emit`.
    pending_emit_len: usize,
    /// Tail of the last synthesised frame's windowed-new-segment — the
    /// SAD matcher target. Length = `overlap_len`. Zeroed before the
    /// first frame.
    last_tail: Vec<f32>,
    /// Sample rate (Hz). Currently informational — not used in the
    /// inner loop but stored for future param-validation extensions.
    #[allow(dead_code)]
    sample_rate: u32,
}

impl Wsola {
    /// Construct a new WSOLA processor.
    ///
    /// **Off-audio-thread**: this allocates the input ring, Hann
    /// window, OLA accumulator, pending-emit queue, and last-tail
    /// cache. Call this once per deck per channel on the control
    /// thread before the audio stream starts.
    ///
    /// # Panics
    ///
    /// * `window_size == 0`.
    /// * `hop_size_out == 0` or `hop_size_out > window_size`.
    /// * `search_range > window_size`.
    pub fn new(
        window_size: usize,
        hop_size_out: usize,
        search_range: usize,
        sample_rate: u32,
    ) -> Self {
        assert!(window_size > 0, "WSOLA window_size must be > 0");
        assert!(
            hop_size_out > 0 && hop_size_out <= window_size,
            "WSOLA hop_size_out must be in (0, window_size]"
        );
        assert!(
            search_range <= window_size,
            "WSOLA search_range must be ≤ window_size"
        );
        let overlap_len = window_size - hop_size_out;
        // Pre-compute Hann window: w[n] = 0.5 × (1 - cos(2πn / (N-1))).
        let mut hann = vec![0.0_f32; window_size];
        let denom = (window_size - 1).max(1) as f32;
        for (i, w) in hann.iter_mut().enumerate() {
            let phase = 2.0 * std::f32::consts::PI * (i as f32) / denom;
            *w = 0.5 * (1.0 - phase.cos());
        }
        // Default analysis hop = synthesis hop (ratio = 1.0,
        // near-passthrough).
        let hop_size_in = hop_size_out;
        Self {
            window_size,
            hop_size_out,
            hop_size_in,
            search_range,
            overlap_len,
            hann,
            in_ring: vec![0.0_f32; RING_LEN],
            in_ring_w: 0,
            in_ring_r: 0,
            oa_buf: vec![0.0_f32; window_size],
            primed: false,
            pending_emit: vec![0.0_f32; hop_size_out],
            pending_emit_len: 0,
            last_tail: vec![0.0_f32; overlap_len],
            sample_rate,
        }
    }

    /// Construct with the module's default parameters
    /// (`DEFAULT_WINDOW_SIZE` / `DEFAULT_HOP_OUT` /
    /// `DEFAULT_SEARCH_RANGE`).
    pub fn with_defaults(sample_rate: u32) -> Self {
        Self::new(
            DEFAULT_WINDOW_SIZE,
            DEFAULT_HOP_OUT,
            DEFAULT_SEARCH_RANGE,
            sample_rate,
        )
    }

    /// Update the time-stretch ratio. **Audio-thread safe** — O(1),
    /// no allocation.
    ///
    /// `ratio > 1.0` = output runs FASTER than input (analysis hop >
    /// synthesis hop). `ratio < 1.0` = slower. The new
    /// `hop_size_in = round(hop_size_out / ratio)`.
    ///
    /// Non-finite or non-positive values fall back to `ratio = 1.0`
    /// (passthrough) so a misbehaving caller never glitches.
    #[inline]
    pub fn set_ratio(&mut self, ratio: f32) {
        let r = if ratio.is_finite() && ratio > 0.0 {
            ratio
        } else {
            1.0
        };
        // hop_size_in = hop_size_out / ratio (rounded). Clamp to [1,
        // hop_size_out × 8] so we never spin forever or trip the
        // ring-buffer headroom guard.
        let raw = (self.hop_size_out as f32 / r).round() as i64;
        let max_hop = (self.hop_size_out * 8) as i64;
        let clamped = raw.clamp(1, max_hop) as usize;
        self.hop_size_in = clamped;
    }

    /// Current input-hop value. Test-only helper.
    #[inline]
    pub fn hop_size_in(&self) -> usize {
        self.hop_size_in
    }

    /// Reset all stateful buffers to their post-`new` state. Useful
    /// when the upstream stream restarts (seek, deck reload). **Audio-
    /// thread safe** — just zeros existing buffers.
    #[inline]
    pub fn reset(&mut self) {
        self.in_ring_w = 0;
        self.in_ring_r = 0;
        self.pending_emit_len = 0;
        self.primed = false;
        for s in self.in_ring.iter_mut() {
            *s = 0.0;
        }
        for s in self.oa_buf.iter_mut() {
            *s = 0.0;
        }
        for s in self.pending_emit.iter_mut() {
            *s = 0.0;
        }
        for s in self.last_tail.iter_mut() {
            *s = 0.0;
        }
    }

    /// Process `input` (mono) into `output` (mono), returning the
    /// number of output samples written.
    ///
    /// **Audio-thread safe** — alloc-free, branch-bounded. Verified
    /// by the `wsola_alloc_free` test.
    ///
    /// The implementation refills the input ring with `input`,
    /// drains any pending-emit samples from a previous call into
    /// `output`, then runs the WSOLA inner loop as long as we have:
    ///
    /// * `>= window_size + search_range` samples ahead of the read
    ///   cursor (enough to run a full SAD search), AND
    /// * `pending_emit_len == 0` (else the next hop has nowhere to
    ///   spill if `output` is also full).
    ///
    /// Unconsumed input stays in the ring; the in-progress overlap
    /// stays in `oa_buf`; overflow-emit samples sit in `pending_emit`
    /// for the next call.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        // 1) Refill the input ring. If incoming would overflow the
        //    ring, drop the oldest in-place — keeps SAD branch-free +
        //    the analysis cursor monotone.
        self.refill_input_ring(input);

        let mut written = 0usize;

        // 2) Drain any pending-emit samples queued by a previous call
        //    whose output buffer ran out mid-hop. These are already
        //    finished — just copy.
        if self.pending_emit_len > 0 {
            let pending = self.pending_emit_len.min(output.len());
            output[..pending].copy_from_slice(&self.pending_emit[..pending]);
            let leftover = self.pending_emit_len - pending;
            if leftover > 0 && pending > 0 {
                self.pending_emit
                    .copy_within(pending..self.pending_emit_len, 0);
            }
            self.pending_emit_len = leftover;
            written += pending;
            // If the caller is still full we can't synthesise more
            // (nowhere to spill new hops).
            if self.pending_emit_len > 0 {
                return written;
            }
        }

        // 3) Inner loop — synthesise as many frames as input permits.
        loop {
            // Stop if pending_emit is non-empty (it must drain to 0
            // before we can stash more).
            if self.pending_emit_len > 0 {
                break;
            }
            // Available analysis samples ahead of the read cursor.
            let avail = self.in_ring_w.saturating_sub(self.in_ring_r);
            if avail < self.window_size + self.search_range {
                break;
            }

            // 3a) SAD search — find the best matcher start offset in
            //     [in_ring_r .. in_ring_r + search_range].
            let best_offset = self.sad_search();
            let analysis_start = self.in_ring_r + best_offset;

            // 3b) Window + overlap-add into oa_buf.
            //     - oa_buf[0..overlap_len] currently holds the
            //       previous frame's tail (already windowed) when
            //       `primed`. The new windowed-head is added in.
            //     - oa_buf[overlap_len..window_size] is overwritten
            //       with the new windowed mid + tail.
            for i in 0..self.window_size {
                let s = self.in_ring[analysis_start + i] * self.hann[i];
                if i < self.overlap_len && self.primed {
                    // Overlap-add: previous frame's tail + new
                    // frame's head.
                    self.oa_buf[i] += s;
                } else {
                    // Pure write — either no previous tail or past
                    // the overlap region.
                    self.oa_buf[i] = s;
                }
            }
            self.primed = true;

            // 3c) Cache the matcher target for the next iteration:
            //     the windowed-NEW-segment tail (NOT the
            //     overlap-added sum) at offset `hop_size_out`. This
            //     is WSOLA's natural-continuation criterion.
            for i in 0..self.overlap_len {
                let src_idx = analysis_start + self.hop_size_out + i;
                self.last_tail[i] = self.in_ring[src_idx] * self.hann[self.hop_size_out + i];
            }

            // 3d) Emit hop_size_out finished samples. Fill caller
            //     first; spill to pending_emit when caller is full.
            for i in 0..self.hop_size_out {
                let s = self.oa_buf[i];
                if written < output.len() {
                    output[written] = s;
                    written += 1;
                } else if self.pending_emit_len < self.pending_emit.len() {
                    self.pending_emit[self.pending_emit_len] = s;
                    self.pending_emit_len += 1;
                }
            }

            // 3e) Slide oa_buf down by hop_size_out: the
            //     [hop_size_out..window_size] region becomes the new
            //     [0..overlap_len] — i.e. the next frame's overlap
            //     prefix.
            self.oa_buf
                .copy_within(self.hop_size_out..self.window_size, 0);
            for s in self.oa_buf[self.overlap_len..self.window_size].iter_mut() {
                *s = 0.0;
            }

            // 3f) Advance the analysis cursor by hop_size_in.
            self.in_ring_r += self.hop_size_in;
            if self.in_ring_r > self.in_ring.len() / 2 {
                self.compact_ring();
            }
        }

        written
    }

    /// Refill the input ring with new samples. Compacts the ring
    /// (drops the already-consumed prefix) if the new data wouldn't
    /// otherwise fit. Defensive against `input.len() > RING_LEN`:
    /// keeps only the most-recent `RING_LEN` samples.
    #[inline]
    fn refill_input_ring(&mut self, input: &[f32]) {
        if input.is_empty() {
            return;
        }
        let ring_len = self.in_ring.len();
        // Defensive cap: if a single chunk exceeds the ring, keep
        // only the most-recent `ring_len` samples (older audio is
        // unavoidably lost — way better than a panic).
        if input.len() >= ring_len {
            let skip = input.len() - ring_len;
            self.in_ring.copy_from_slice(&input[skip..]);
            self.in_ring_w = ring_len;
            self.in_ring_r = 0;
            return;
        }
        if self.in_ring_w + input.len() > ring_len {
            self.compact_ring();
        }
        if self.in_ring_w + input.len() > ring_len {
            // Still won't fit even after compaction — drop the oldest
            // unconsumed samples. Overflow ≤ input.len() < ring_len
            // and ≤ in_ring_w (because in_ring_w + input.len() >
            // ring_len ⇒ in_ring_w > ring_len - input.len() ≥ 0,
            // and overflow = in_ring_w + input.len() - ring_len ≤
            // in_ring_w).
            let overflow = (self.in_ring_w + input.len()) - ring_len;
            if overflow < self.in_ring_w {
                self.in_ring.copy_within(overflow..self.in_ring_w, 0);
            }
            self.in_ring_w = self.in_ring_w.saturating_sub(overflow);
            self.in_ring_r = self.in_ring_r.saturating_sub(overflow);
        }
        let dst = &mut self.in_ring[self.in_ring_w..self.in_ring_w + input.len()];
        dst.copy_from_slice(input);
        self.in_ring_w += input.len();
    }

    /// Slide unread input down to index 0 to keep cursors bounded.
    /// Alloc-free.
    #[inline]
    fn compact_ring(&mut self) {
        let r = self.in_ring_r;
        if r == 0 {
            return;
        }
        let live = self.in_ring_w - r;
        if live > 0 {
            self.in_ring.copy_within(r..self.in_ring_w, 0);
        }
        // Zero the freed tail so stale samples don't leak into
        // subsequent SAD searches.
        for s in self.in_ring[live..self.in_ring_w].iter_mut() {
            *s = 0.0;
        }
        self.in_ring_w = live;
        self.in_ring_r = 0;
    }

    /// SAD = sum of absolute differences. Search the `search_range`
    /// samples starting at `in_ring_r` for the offset whose first
    /// `overlap_len` windowed samples best match `last_tail`.
    ///
    /// Returns the offset in `[0, search_range]` that minimised SAD.
    /// Returns 0 on the first call (when `last_tail` is all zeros —
    /// any offset minimises SAD equally, 0 is the canonical pick).
    ///
    /// **Audio-thread safe** — pure compute, no allocation.
    #[inline]
    fn sad_search(&self) -> usize {
        // First-frame shortcut: last_tail is all zeros → SAD is
        // constant across offsets, pick 0 to skip pointless work.
        if !self.primed {
            return 0;
        }
        let mut best_offset = 0usize;
        let mut best_score = f32::INFINITY;
        for offset in 0..=self.search_range {
            let mut score = 0.0_f32;
            let base = self.in_ring_r + offset;
            for i in 0..self.overlap_len {
                let s = self.in_ring[base + i] * self.hann[i];
                score += (s - self.last_tail[i]).abs();
                // Early-out: skip the rest of this offset if we're
                // already over best_score. Cuts typical SAD cost
                // roughly in half.
                if score >= best_score {
                    break;
                }
            }
            if score < best_score {
                best_score = score;
                best_offset = offset;
            }
        }
        best_offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `freq`-Hz mono sine of `n` samples at `sr`.
    fn sine(freq: f32, sr: u32, n: usize) -> Vec<f32> {
        let tau = std::f32::consts::TAU;
        (0..n)
            .map(|i| (tau * freq * (i as f32 / sr as f32)).sin())
            .collect()
    }

    /// Naive zero-crossing-rate estimator (positive-going crossings
    /// per second). Good enough to verify pitch preservation: a 440 Hz
    /// sine has ZCR ≈ 440 regardless of how much we time-stretch.
    fn zcr(samples: &[f32], sr: u32) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let mut prev = 0.0_f32;
        let mut crossings = 0_u32;
        for &s in samples {
            if prev <= 0.0 && s > 0.0 {
                crossings += 1;
            }
            prev = s;
        }
        (crossings as f32) * (sr as f32) / (samples.len() as f32)
    }

    /// Drain `input` through `w` in 1024-sample chunks, collecting up
    /// to `min_out` output samples. Useful for getting a steady-state
    /// signal long enough for ZCR measurement.
    fn drive(w: &mut Wsola, input: &[f32], min_out: usize) -> Vec<f32> {
        let mut out_buf = vec![0.0_f32; min_out + 2048];
        let mut written = 0;
        for chunk in input.chunks(1024) {
            let n = w.process(chunk, &mut out_buf[written..]);
            written += n;
            if written >= min_out {
                break;
            }
        }
        out_buf.truncate(written);
        out_buf
    }

    #[test]
    fn wsola_at_default_ratio_passthrough() {
        // ratio = 1.0 → analysis hop == synthesis hop → output should
        // approximate the input. Tolerances are loose because WSOLA's
        // overlap-add introduces a startup transient (the first
        // frame has no predecessor so it single-windows rather than
        // fading in from zero).
        let mut w = Wsola::with_defaults(48_000);
        w.set_ratio(1.0);
        let input = sine(440.0, 48_000, 8192);
        let output = drive(&mut w, &input, 2048);
        assert!(
            output.len() >= 2048,
            "ratio=1 produced too little output: {}",
            output.len()
        );
        // Pitch unchanged: ZCR should be ≈ 440. Allow ±15% slack
        // because overlap-add startup attenuates a few crossings.
        let z = zcr(&output, 48_000);
        assert!(
            (z - 440.0).abs() < 80.0,
            "ratio=1 passthrough ZCR drifted from 440Hz: got {z}"
        );
    }

    #[test]
    fn wsola_pitch_preserved_with_tempo_stretch() {
        // The key correctness property: a 2× time-stretch (slow down)
        // must NOT halve the pitch (which is what naive resampling
        // would do — that's the exact bug WSOLA fixes).
        let mut w = Wsola::with_defaults(48_000);
        w.set_ratio(0.5); // 2× slower
        let input = sine(440.0, 48_000, 32_768);
        let output = drive(&mut w, &input, 4096);
        assert!(
            output.len() >= 4096,
            "ratio=0.5 didn't produce enough output"
        );
        // Skip the first 1024 samples (startup transient + Hann ramp).
        let steady = &output[1024..];
        // Naive resampling at 2× slower would produce a 220 Hz sine
        // (ZCR ≈ 220). WSOLA must keep ZCR around 440. Allow ±25%.
        let z = zcr(steady, 48_000);
        assert!(
            (z - 440.0).abs() < 110.0,
            "WSOLA failed to preserve pitch under 2× stretch: ZCR = {z} (expected ≈ 440)"
        );
    }

    #[test]
    fn wsola_speed_up_2x_preserves_pitch() {
        // Same property in the other direction: 2× speed-up must NOT
        // double the pitch.
        let mut w = Wsola::with_defaults(48_000);
        w.set_ratio(2.0);
        let input = sine(440.0, 48_000, 32_768);
        let output = drive(&mut w, &input, 4096);
        assert!(
            output.len() >= 2048,
            "ratio=2.0 didn't produce enough output"
        );
        let steady = &output[1024..];
        let z = zcr(steady, 48_000);
        assert!(
            (z - 440.0).abs() < 110.0,
            "WSOLA failed to preserve pitch under 2× speed-up: ZCR = {z} (expected ≈ 440)"
        );
    }

    #[test]
    fn wsola_alloc_free() {
        // ADR-004 compliance: process() must not allocate on the
        // audio thread.
        let mut w = Wsola::with_defaults(48_000);
        w.set_ratio(0.75);
        let input = vec![0.1_f32; 1024];
        let mut output = vec![0.0_f32; 1024];
        // Prime: first call sets up internal state.
        let _ = w.process(&input, &mut output);
        assert_no_alloc::assert_no_alloc(|| {
            let _ = w.process(&input, &mut output);
        });
    }

    #[test]
    fn wsola_at_extreme_ratio_doesnt_panic() {
        // Both ends of the supported range (and beyond): no panic +
        // bounded output.
        for ratio in [0.25_f32, 0.5, 1.0, 2.0, 4.0] {
            let mut w = Wsola::with_defaults(48_000);
            w.set_ratio(ratio);
            let input = sine(440.0, 48_000, 4096);
            let mut output = vec![0.0_f32; 8192];
            let n = w.process(&input, &mut output);
            assert!(
                n <= output.len(),
                "ratio={ratio}: wrote {n} samples > buffer {}",
                output.len()
            );
            // At ratio ≥ 0.5 the test feed produces ≥ 1 frame.
            if ratio >= 0.5 {
                assert!(n > 0, "ratio={ratio}: no output produced");
            }
        }
    }

    #[test]
    fn wsola_window_overlap_correctness() {
        // Feed an impulse train at known spacing; verify the output
        // contains non-trivial energy. Regression for the overlap-add
        // path swallowing peaks to zero or amplifying them
        // pathologically (a clipped / mis-windowed path can amplify
        // 100× — both ends are unacceptable).
        let mut w = Wsola::with_defaults(48_000);
        w.set_ratio(1.0);
        let mut input = vec![0.0_f32; 8192];
        // Impulse every 256 samples (matches hop_out).
        for i in (0..input.len()).step_by(256) {
            input[i] = 1.0;
        }
        let output = drive(&mut w, &input, 2048);
        let energy: f32 = output.iter().map(|s| s * s).sum();
        assert!(
            energy > 0.01,
            "overlap-add cancelled impulse train: energy = {energy}"
        );
        assert!(
            energy < 200.0,
            "overlap-add amplified impulse train pathologically: energy = {energy}"
        );
    }

    #[test]
    fn wsola_silent_input_silent_output() {
        // No NaN / explosion on all-zeros input.
        let mut w = Wsola::with_defaults(48_000);
        w.set_ratio(0.7);
        let input = vec![0.0_f32; 4096];
        let mut output = vec![1.0_f32; 4096]; // sentinel
        let n = w.process(&input, &mut output);
        for (i, s) in output[..n].iter().enumerate() {
            assert!(
                s.abs() < 1e-6,
                "silent input produced non-silent output at {i}: {s}"
            );
            assert!(s.is_finite(), "non-finite at {i}: {s}");
        }
    }

    #[test]
    fn wsola_set_ratio_updates_hop_size_in() {
        let mut w = Wsola::with_defaults(48_000);
        // ratio = 1.0 → hop_size_in = hop_size_out = 512
        w.set_ratio(1.0);
        assert_eq!(w.hop_size_in(), 512);
        // ratio = 2.0 → hop_size_in = 256 (analysis hop is half →
        // output runs 2× faster).
        w.set_ratio(2.0);
        assert_eq!(w.hop_size_in(), 256);
        // ratio = 0.5 → hop_size_in = 1024
        w.set_ratio(0.5);
        assert_eq!(w.hop_size_in(), 1024);
        // NaN → falls back to ratio = 1.0
        w.set_ratio(f32::NAN);
        assert_eq!(w.hop_size_in(), 512);
        // Negative → falls back to ratio = 1.0
        w.set_ratio(-1.0);
        assert_eq!(w.hop_size_in(), 512);
    }

    #[test]
    fn wsola_reset_clears_state() {
        let mut w = Wsola::with_defaults(48_000);
        w.set_ratio(0.5);
        let input = sine(440.0, 48_000, 4096);
        let mut output = vec![0.0_f32; 4096];
        let _ = w.process(&input, &mut output);
        w.reset();
        assert!(w.in_ring.iter().all(|s| *s == 0.0));
        assert!(w.oa_buf.iter().all(|s| *s == 0.0));
        assert!(w.pending_emit.iter().all(|s| *s == 0.0));
        assert!(w.last_tail.iter().all(|s| *s == 0.0));
        assert_eq!(w.in_ring_w, 0);
        assert_eq!(w.in_ring_r, 0);
        assert_eq!(w.pending_emit_len, 0);
        assert!(!w.primed);
    }

    /// Latency probe — `process` with a 1024-sample input must
    /// complete inside the audio-thread budget.
    ///
    /// ADR-004 caps the WHOLE audio callback at ≤ 50% of the
    /// per-buffer budget (≈ 2.6 ms at 256 frames / 48 kHz). WSOLA
    /// gets one slice of that budget alongside pitch resample,
    /// effects, mix, limiter. We target ≤ 500 µs worst case across
    /// 200 release-build iterations — leaves ≥ 2 ms headroom for
    /// the rest of the callback.
    ///
    /// Debug builds run the same assertion but with a relaxed budget
    /// (debug-build SAD is ~10× slower without LLVM auto-vec).
    #[test]
    fn wsola_process_latency_under_budget() {
        let mut w = Wsola::with_defaults(48_000);
        w.set_ratio(0.7);
        let input = sine(440.0, 48_000, 1024);
        let mut output = vec![0.0_f32; 2048];
        // Prime: a few iterations to fill ring + run the SAD path.
        for _ in 0..6 {
            let _ = w.process(&input, &mut output);
        }
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..200 {
            let t = std::time::Instant::now();
            let _ = w.process(&input, &mut output);
            let dt = t.elapsed();
            if dt > worst {
                worst = dt;
            }
        }
        let budget = if cfg!(debug_assertions) {
            std::time::Duration::from_millis(3)
        } else {
            std::time::Duration::from_micros(500)
        };
        eprintln!("[wsola latency] 1024-sample process worst-case: {worst:?} (budget {budget:?})");
        assert!(
            worst <= budget,
            "WSOLA process exceeded budget: worst {worst:?} > {budget:?}"
        );
    }
}
