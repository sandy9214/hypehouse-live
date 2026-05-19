//! Decode service — symphonia-backed streaming.
//!
//! # Architecture
//!
//! ADR-004 forbids any heap allocation, syscall, or blocking primitive on
//! the audio thread. Symphonia decode + rubato resample both allocate
//! freely. The compromise this module enforces:
//!
//! ```text
//!   control thread                 decoder thread (per track)
//!   ┌────────────┐  spawn()        ┌─────────────────────────────┐
//!   │ open(track)│ ───────────────▶│ symphonia decode → rubato → │
//!   │            │                  │ stereo f32 interleaved      │
//!   │            │                  │   push into SPSC ring       │
//!   └────────────┘                  └──────────────┬──────────────┘
//!                                                  │
//!                                          ┌───────▼───────┐
//!                                          │ ArrayQueue    │
//!                                          │ (~500ms @48k) │
//!                                          └───────┬───────┘
//!                                                  │ (consumer side)
//!                                                  ▼
//!                                            audio thread
//!                                            read(handle, buf):
//!                                              pure lock-free pop
//! ```
//!
//! The audio thread's `read(handle, &mut buf)` is alloc-free,
//! syscall-free, and never blocks. If the decoder thread falls behind
//! (slow disk, OS scheduling jitter), the ring underruns and `read`
//! pads the remainder of the caller's buffer with `0.0` (silence) and
//! increments `underrun_count` for observability.
//!
//! # DecodeHandle
//!
//! `DecodeHandle` is a `Copy` index into a fixed-size slot table held
//! inside `SymphoniaDecodeService`. Both the control thread (open/close)
//! and the audio thread (read) hold cheap `Arc` clones of the same
//! service; the slot table is shared via atomics. Each slot owns its
//! SPSC ring **for the lifetime of the service** — never re-allocated
//! — so the audio thread can read `slot.queue` without any
//! synchronization primitive beyond the ring's own lock-free pop.
//!
//! # Format support
//!
//! `symphonia` is built with `features = ["all"]`: mp3, m4a/aac, wav,
//! flac, ogg/vorbis. Sample format is normalized to interleaved stereo
//! f32 in `[-1.0, 1.0]`. Mono sources are duplicated to both channels;
//! Channel counts above 2 are downmixed L=ch0, R=ch1 (v0.1 — rears are
//! dropped, see ADR-002 council on multichannel handling).
//!
//! # Resampling
//!
//! When `source_sr != target_sr`, a per-track `rubato::SincFixedIn`
//! resampler converts in fixed-size chunks. Settings match the
//! symphonia upstream recommendation: `sinc_len=256`, `f_cutoff=0.95`,
//! `oversampling_factor=256`, `interpolation=Linear`,
//! `window=BlackmanHarris2`.
//!
//! See `docs/adr/ADR-004-audio-thread.md` for the full rationale.

use std::collections::HashMap;
use std::fs::File;
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam::channel::{bounded, Receiver, Sender, TrySendError};
use crossbeam::queue::ArrayQueue;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use symphonia::core::audio::AudioBufferRef;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::state::TrackRef;

/// Maximum number of simultaneously-open decoder slots. The engine has
/// 2 decks; we reserve headroom for queued preloads + future
/// auxiliary decks. Bumping requires an ADR (audio-thread slot scan
/// becomes hotter; per-slot fixed ring is pre-allocated).
pub const MAX_DECODE_SLOTS: usize = 16;

/// Ring capacity in **interleaved samples** — 500 ms @ 48 kHz stereo.
/// = 48_000 frames * 0.5 s * 2 channels = 48_000 samples per slot.
pub const RING_SAMPLES_500MS: usize = 48_000;

/// In-memory test source prefix. Paths starting with `mem://<key>`
/// look up `<key>` in the service's inline-source registry instead of
/// touching the filesystem. Used by unit + smoke tests.
pub const MEM_PREFIX: &str = "mem://";

/// Sidechannel capacity for mid-stream decoder failures. Sized to
/// absorb a multi-deck failure storm (e.g. every open slot fails on
/// the same corrupt-format wave) without back-pressuring the decoder
/// thread or losing events. The bridge drains on a 100ms cadence, so
/// 64 events of headroom = ~6.4s of catch-up at one failure per tick
/// per slot — comfortably more than the engine ever expects to see.
///
/// On overflow the decoder thread drops the event and logs a `warn`;
/// the in-thread `try_send` is non-blocking by design (the decoder
/// thread must never block on a slow consumer).
pub const MID_STREAM_FAILURE_CAPACITY: usize = 64;

/// Wire-facing category emitted when symphonia returns a decode error
/// mid-track (after a successful `open`). Surfaces as the `category`
/// field on the `engine.decode_error` notification.
pub const MID_STREAM_CATEGORY: &str = "mid_stream_decode_failure";

/// Wire-facing category emitted when the decoder thread itself panics
/// and the `catch_unwind` guard recovers. Surfaces as the `category`
/// field on the `engine.decode_error` notification. The audio thread
/// continues to silence-pad the ring (no crash, no `unsafe`).
pub const DECODER_THREAD_PANIC_CATEGORY: &str = "decoder_thread_panic";

/// Source of a mid-stream failure observed by the per-track decoder
/// thread AFTER `DecodeService::open` returned successfully.
///
/// Open-time failures (file not found, format unsupported) are
/// surfaced synchronously via `DecodeError` on `open` — see PR #56.
/// This enum covers the asynchronous failure modes the decoder thread
/// hits AFTER it's been spawned, which the original PR #56 silently
/// silence-padded.
#[derive(Debug)]
pub enum MidStreamFailureKind {
    /// Symphonia decode returned a non-recoverable error mid-track
    /// (corrupt frame outside `DecodeError`, unsupported codec
    /// extension reached after the first packet, etc.). Maps to the
    /// wire category [`MID_STREAM_CATEGORY`].
    DecodeFailed(String),
    /// Rubato resample errored on a mid-stream chunk (e.g. NaN /
    /// non-finite input). Maps to wire category [`MID_STREAM_CATEGORY`]
    /// because the symptom (silent ring) is identical from the UI's
    /// point of view.
    ResampleFailed(String),
    /// The decoder thread itself panicked and `catch_unwind` caught
    /// the unwind. Maps to wire category
    /// [`DECODER_THREAD_PANIC_CATEGORY`]. The thread exits cleanly
    /// after publishing — the audio thread continues silence-padding
    /// the ring (existing PR #29 contract).
    ThreadPanic(String),
}

impl MidStreamFailureKind {
    /// Stable wire-facing category string. See
    /// `docs/api/ws-protocol.md` "Engine notifications: decode_error".
    pub fn category(&self) -> &'static str {
        match self {
            MidStreamFailureKind::DecodeFailed(_) | MidStreamFailureKind::ResampleFailed(_) => {
                MID_STREAM_CATEGORY
            }
            MidStreamFailureKind::ThreadPanic(_) => DECODER_THREAD_PANIC_CATEGORY,
        }
    }

    /// Human-readable message for the `error` wire field.
    pub fn message(&self) -> &str {
        match self {
            MidStreamFailureKind::DecodeFailed(s)
            | MidStreamFailureKind::ResampleFailed(s)
            | MidStreamFailureKind::ThreadPanic(s) => s,
        }
    }
}

/// Sidechannel payload pushed by the decoder thread when an
/// after-open failure occurs.
///
/// The `track_id` is recorded at thread spawn so the bridge can
/// surface it to the UI without re-correlating with engine state.
/// The optional `deck` is populated when the open-side knows which
/// deck the load was targeting (every production open currently
/// does); when missing the bridge omits it from the wire payload.
#[derive(Debug)]
pub struct MidStreamFailure {
    /// Track id from the original `DeckLoad`. Always populated.
    pub track_id: String,
    /// Optional deck id. Surfaces on the wire as `deck` when present.
    /// Stored as a stable `'A' | 'B' | …` char so this module stays
    /// free of any dep on `crate::state::DeckId`.
    pub deck: Option<char>,
    /// Concrete failure kind.
    pub kind: MidStreamFailureKind,
}

/// Errors from the decode pipeline. All variants are non-fatal at the
/// engine level — a failed open returns an `Err` and the deck simply
/// doesn't load.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("decode slot table exhausted (max {MAX_DECODE_SLOTS})")]
    NoFreeSlot,
    #[error("io error opening {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("symphonia probe failed: {0}")]
    Probe(String),
    #[error("no default audio track in container")]
    NoTrack,
    #[error("rubato resampler init failed: {0}")]
    Resampler(String),
    #[error("track id `{0}` is reserved for inline test sources but no source registered")]
    UnknownInlineSource(String),
    #[error("failed to spawn decoder thread: {0}")]
    Spawn(std::io::Error),
}

/// Opaque, `Copy` handle into the decoder slot table. Cheap to put
/// inside an `AudioCommand` (POD).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DecodeHandle(pub u32);

impl DecodeHandle {
    /// Sentinel used to indicate "no track loaded".
    pub const NONE: DecodeHandle = DecodeHandle(u32::MAX);

    pub fn is_some(&self) -> bool {
        self.0 != u32::MAX
    }
}

/// 4-up handle for stem-aware playback (vocals/drums/bass/other).
///
/// Each entry is an independent `DecodeHandle` into the underlying
/// service's slot table — the stem-aware playback path opens four
/// regular decode slots (one per WAV) and bundles them in a single
/// POD struct for the audio thread. The thread reads each stem via
/// [`DecodeService::read_stem`] (which dispatches to the underlying
/// `read` for the indexed slot) and mixes them per-block with the
/// deck's `stem_gains` envelope.
///
/// `Copy + Send + Sync + 'static` so it fits in an `AudioCommand`
/// variant. Total size = 4 × 4 bytes = 16 bytes; the per-stem
/// `DecodeHandle::NONE` sentinel marks unused slots (today we always
/// fill all four — partial fills are a future-proofing concession).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StemHandle(pub [DecodeHandle; 4]);

impl StemHandle {
    /// Sentinel "no stems loaded".
    pub const NONE: StemHandle = StemHandle([DecodeHandle::NONE; 4]);

    /// True if at least one stem slot is filled.
    pub fn is_some(&self) -> bool {
        self.0.iter().any(|h| h.is_some())
    }

    /// Borrow the underlying `DecodeHandle` for `stem_idx` (0..4).
    /// Returns `DecodeHandle::NONE` for out-of-range indices.
    #[inline]
    pub fn get(&self, stem_idx: usize) -> DecodeHandle {
        if stem_idx < 4 {
            self.0[stem_idx]
        } else {
            DecodeHandle::NONE
        }
    }
}

/// Decoder-side trait. The control thread calls `open` + `close`; the
/// audio thread calls `read` (alloc-free, no syscalls, no blocking).
///
/// Implementations are typically wrapped in `Arc` and cloned to both
/// threads.
pub trait DecodeService: Send + Sync {
    /// Open a track. Spawns the decoder thread off the control plane.
    /// Returns a handle whose subsequent `read`s pull stereo f32 frames.
    /// **Control thread only** — may allocate, do I/O, etc.
    fn open(&self, track: &TrackRef, target_sample_rate: u32) -> Result<DecodeHandle, DecodeError>;

    /// Read up to `buf.len()` interleaved stereo f32 samples for the
    /// given handle. Returns the number of f32 samples written.
    /// Tail is filled with `0.0` on underrun.
    ///
    /// **Audio-thread safe**: no alloc, no syscalls, no blocking.
    fn read(&self, handle: DecodeHandle, buf: &mut [f32]) -> usize;

    /// Close the handle. Signals shutdown to the decoder thread and
    /// joins it. **Control thread only.**
    fn close(&self, handle: DecodeHandle);

    /// Total underrun events across all handles since service start
    /// (one event per render call that had to silence-pad).
    fn underrun_count(&self) -> u64;

    /// Open four stem WAVs (vocals/drums/bass/other) as a single
    /// bundled handle. Each stem is opened via the same path as a
    /// regular `open` so the underlying slot table is shared — this
    /// consumes **4 slots**. If any single stem open fails, ALL
    /// previously-opened stems in the same call are closed before
    /// the error is returned (atomic open guarantee).
    ///
    /// **Control thread only.**
    ///
    /// Default implementation delegates to `open()` four times +
    /// rolls back on partial failure; production services may
    /// override for batching but the contract is identical.
    fn open_stems(
        &self,
        track: &TrackRef,
        stem_paths: &[String; 4],
        target_sample_rate: u32,
    ) -> Result<StemHandle, DecodeError> {
        let mut handles = [DecodeHandle::NONE; 4];
        for (i, path) in stem_paths.iter().enumerate() {
            // Decorate the track id so the spawned decoder thread
            // has a recognisable name (`hh-decode-<id>::stem<i>`)
            // and trace-level diagnostics distinguish stems.
            let stem_ref = TrackRef {
                id: format!("{}::stem{}", track.id, i),
                path: path.clone(),
            };
            match self.open(&stem_ref, target_sample_rate) {
                Ok(h) => handles[i] = h,
                Err(e) => {
                    // Roll back any stems already opened in this call.
                    for h in handles.iter().take(i) {
                        if h.is_some() {
                            self.close(*h);
                        }
                    }
                    return Err(e);
                }
            }
        }
        Ok(StemHandle(handles))
    }

    /// Read `buf.len()` interleaved stereo f32 samples from the
    /// `stem_idx`-th stem in `handle`. Returns the number of samples
    /// written. Tail filled with `0.0` on underrun (or silently if
    /// the requested stem is unbound — `DecodeHandle::NONE`).
    ///
    /// **Audio-thread safe**: alloc-free; defers to the underlying
    /// `read()` for the indexed `DecodeHandle`.
    fn read_stem(&self, handle: StemHandle, stem_idx: usize, buf: &mut [f32]) -> usize {
        let h = handle.get(stem_idx);
        if !h.is_some() {
            // Silent zero-fill so the mixer can MAC against a quiet
            // buffer instead of branching per sample.
            for s in buf.iter_mut() {
                *s = 0.0;
            }
            return buf.len();
        }
        self.read(h, buf)
    }

    /// Close all 4 stems in `handle`. Idempotent — calling on an
    /// already-closed handle is a no-op.
    ///
    /// **Control thread only.**
    fn close_stems(&self, handle: StemHandle) {
        for h in handle.0.iter() {
            if h.is_some() {
                self.close(*h);
            }
        }
    }

    /// Receiver end of the mid-stream-failure sidechannel.
    ///
    /// The bridge holds this and drains it on a 100ms cadence,
    /// publishing each event as an `engine.decode_error` notification.
    /// Default implementation returns `None` so test stubs that don't
    /// need the channel can opt out cheaply.
    ///
    /// Returns `None` once the receiver has been claimed (the
    /// sidechannel has a single consumer — the bridge — and the
    /// service hands the receiver out exactly once at wire-up).
    fn take_mid_stream_failure_receiver(&self) -> Option<Receiver<MidStreamFailure>> {
        None
    }
}

// ---------------------------------------------------------------------------
// SymphoniaDecodeService
// ---------------------------------------------------------------------------

/// Per-slot state. The `queue` Arc is allocated ONCE at service
/// construction and reused across opens — this is what lets the audio
/// thread read without any lock or atomic swap.
struct Slot {
    /// `false` = free, `true` = open. Used by `open` to claim
    /// (`compare_exchange`) and by `close` to release. The audio
    /// thread checks this on `read` to short-circuit dead slots.
    occupied: AtomicBool,
    /// SPSC ring. The decoder thread holds an `Arc` clone, the audio
    /// thread accesses via `&self`. `ArrayQueue::push` and `pop` are
    /// lock-free.
    queue: Arc<ArrayQueue<f32>>,
    /// Per-decoder-thread shutdown flag.
    shutdown: Arc<AtomicBool>,
    /// JoinHandle for the per-slot decoder thread. Touched only on the
    /// control thread (open/close); never on the audio thread.
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Slot {
    fn new() -> Self {
        Self {
            occupied: AtomicBool::new(false),
            queue: Arc::new(ArrayQueue::new(RING_SAMPLES_500MS)),
            shutdown: Arc::new(AtomicBool::new(false)),
            join: Mutex::new(None),
        }
    }
}

/// Shared inner state. Both the control-side handle and the
/// audio-side handle hold `Arc<SymphoniaShared>` so the slot table is
/// visible to both threads.
struct SymphoniaShared {
    slots: Vec<Slot>,
    next_id: AtomicU64,
    underruns: AtomicU64,
    inline_sources: Mutex<HashMap<String, Vec<u8>>>,
    /// Sender end of the mid-stream-failure sidechannel. Cloned into
    /// every spawned decoder thread; the receiver is owned by the
    /// bridge drain task (see `bridge::decode_drain`).
    mid_stream_tx: Sender<MidStreamFailure>,
    /// Receiver, parked until [`take_mid_stream_failure_receiver`]
    /// hands it to the bridge. `Mutex<Option<_>>` instead of
    /// `OnceLock` so tests can also peek without consuming if needed.
    mid_stream_rx: Mutex<Option<Receiver<MidStreamFailure>>>,
}

impl SymphoniaShared {
    fn new() -> Self {
        let mut slots = Vec::with_capacity(MAX_DECODE_SLOTS);
        for _ in 0..MAX_DECODE_SLOTS {
            slots.push(Slot::new());
        }
        let (tx, rx) = bounded::<MidStreamFailure>(MID_STREAM_FAILURE_CAPACITY);
        Self {
            slots,
            next_id: AtomicU64::new(1),
            underruns: AtomicU64::new(0),
            inline_sources: Mutex::new(HashMap::new()),
            mid_stream_tx: tx,
            mid_stream_rx: Mutex::new(Some(rx)),
        }
    }
}

/// Production decode service backed by symphonia + rubato.
///
/// Cloning is cheap (`Arc` refcount bump). Hand one clone to the
/// control thread (for `open`/`close`) and another to the audio thread
/// (for `read`).
#[derive(Clone)]
pub struct SymphoniaDecodeService {
    inner: Arc<SymphoniaShared>,
}

impl Default for SymphoniaDecodeService {
    fn default() -> Self {
        Self::new()
    }
}

impl SymphoniaDecodeService {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SymphoniaShared::new()),
        }
    }

    /// Register an in-memory source under a synthetic `mem://<key>`
    /// path. Used by tests to decode without touching the filesystem.
    pub fn register_inline_source(&self, key: &str, bytes: Vec<u8>) {
        let mut guard = self.inner.inline_sources.lock().expect("poisoned");
        guard.insert(key.to_string(), bytes);
    }

    /// Number of currently-occupied slots. O(MAX_DECODE_SLOTS).
    pub fn open_slot_count(&self) -> usize {
        self.inner
            .slots
            .iter()
            .filter(|s| s.occupied.load(Ordering::Acquire))
            .count()
    }
}

impl DecodeService for SymphoniaDecodeService {
    fn open(&self, track: &TrackRef, target_sample_rate: u32) -> Result<DecodeHandle, DecodeError> {
        // 1. Find a free slot via compare_exchange.
        let slot_idx = self
            .inner
            .slots
            .iter()
            .position(|s| {
                s.occupied
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            })
            .ok_or(DecodeError::NoFreeSlot)?;

        let slot = &self.inner.slots[slot_idx];

        // 2. Reset the per-slot shutdown flag + drain any residual
        //    samples from a previous owner. (queue is reused across
        //    opens by design.)
        slot.shutdown.store(false, Ordering::Release);
        while slot.queue.pop().is_some() {}

        // 3. Open the source (file or inline blob). Errors bubble up;
        //    we must un-mark the slot in that case.
        let mss = match open_media_source(&track.path, &self.inner.inline_sources) {
            Ok(m) => m,
            Err(e) => {
                slot.occupied.store(false, Ordering::Release);
                return Err(e);
            }
        };

        // 4. Spawn the decoder thread.
        let ring = Arc::clone(&slot.queue);
        let shutdown = Arc::clone(&slot.shutdown);
        let failure_tx = self.inner.mid_stream_tx.clone();
        let track_id_owned = track.id.clone();
        let join = std::thread::Builder::new()
            .name(format!("hh-decode-{}", track.id))
            .spawn(move || {
                decoder_thread_main(
                    mss,
                    target_sample_rate,
                    ring,
                    shutdown,
                    failure_tx,
                    track_id_owned,
                );
            })
            .map_err(|e| {
                slot.occupied.store(false, Ordering::Release);
                DecodeError::Spawn(e)
            })?;

        // 5. Store the join handle on the slot.
        {
            let mut guard = slot.join.lock().expect("slot join mutex poisoned");
            *guard = Some(join);
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) as u32;
        Ok(DecodeHandle(encode_handle(slot_idx, id)))
    }

    fn read(&self, handle: DecodeHandle, buf: &mut [f32]) -> usize {
        if !handle.is_some() || buf.is_empty() {
            return 0;
        }
        let (slot_idx, _id) = decode_handle(handle);
        if slot_idx >= self.inner.slots.len() {
            return 0;
        }
        let slot = &self.inner.slots[slot_idx];
        if !slot.occupied.load(Ordering::Acquire) {
            return 0;
        }

        // Pure lock-free pop loop — no allocation, no syscall.
        let mut written = 0usize;
        while written < buf.len() {
            match slot.queue.pop() {
                Some(sample) => {
                    buf[written] = sample;
                    written += 1;
                }
                None => break,
            }
        }
        if written < buf.len() {
            // Underrun: zero-fill the rest. Count one event per
            // affected `read` call.
            for s in &mut buf[written..] {
                *s = 0.0;
            }
            self.inner.underruns.fetch_add(1, Ordering::Relaxed);
        }
        buf.len()
    }

    fn close(&self, handle: DecodeHandle) {
        if !handle.is_some() {
            return;
        }
        let (slot_idx, _id) = decode_handle(handle);
        if slot_idx >= self.inner.slots.len() {
            return;
        }
        let slot = &self.inner.slots[slot_idx];
        if !slot.occupied.load(Ordering::Acquire) {
            return;
        }

        // 1. Signal the decoder thread to exit. The decoder polls
        //    shutdown between every packet + every push spin.
        slot.shutdown.store(true, Ordering::Release);

        // 2. Drain a bit of the queue so the decoder isn't blocked on
        //    a full ring at the moment we signal shutdown. The
        //    decoder also checks shutdown inside its push loop, so
        //    this is belt-and-braces.
        for _ in 0..(RING_SAMPLES_500MS / 8) {
            if slot.queue.pop().is_none() {
                break;
            }
        }

        // 3. Take + join the thread.
        let join = {
            let mut g = slot.join.lock().expect("slot join mutex poisoned");
            g.take()
        };
        if let Some(j) = join {
            let _ = j.join();
        }

        // 4. Drain any leftover samples so the ring is empty for next
        //    open().
        while slot.queue.pop().is_some() {}

        slot.occupied.store(false, Ordering::Release);
    }

    fn underrun_count(&self) -> u64 {
        self.inner.underruns.load(Ordering::Relaxed)
    }

    fn take_mid_stream_failure_receiver(&self) -> Option<Receiver<MidStreamFailure>> {
        let mut slot = self
            .inner
            .mid_stream_rx
            .lock()
            .expect("mid_stream_rx poisoned");
        slot.take()
    }
}

impl SymphoniaDecodeService {
    /// Test/internal helper: push a synthetic mid-stream failure onto
    /// the sidechannel as if the decoder thread had observed one.
    /// Used by integration tests + the test-only injectable-decoder
    /// path to exercise the drain plumbing without standing up a real
    /// symphonia pipeline. Visible inside the crate (and tests) only.
    #[doc(hidden)]
    pub fn __inject_mid_stream_failure_for_test(&self, failure: MidStreamFailure) {
        try_send_mid_stream_failure(&self.inner.mid_stream_tx, failure);
    }

    /// Test/internal helper: spawn a custom decoder-style thread that
    /// pushes onto the same sidechannel + ring as the production
    /// decoder, wrapped in the same `catch_unwind` guard. Lets us
    /// test panic recovery + mid-stream decode-failure flow without
    /// depending on symphonia returning a specific error.
    ///
    /// The closure receives `(ring, shutdown, failure_tx, track_id)`
    /// and is responsible for pushing samples + failures itself. The
    /// outer wrapper handles `catch_unwind` so a panic inside `body`
    /// is converted to a `ThreadPanic` failure exactly the same way
    /// the production path does.
    #[doc(hidden)]
    pub fn __spawn_test_decoder<F>(&self, track_id: String, body: F) -> JoinHandle<()>
    where
        F: FnOnce(Arc<ArrayQueue<f32>>, Arc<AtomicBool>, Sender<MidStreamFailure>, String)
            + Send
            + 'static,
    {
        // Claim the first free slot so the spawned thread has a real
        // ring to push into; we still set `occupied=true` so the
        // public state reflects "this slot is in use".
        let slot_idx = self
            .inner
            .slots
            .iter()
            .position(|s| {
                s.occupied
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            })
            .expect("no free slot for test decoder");
        let slot = &self.inner.slots[slot_idx];
        slot.shutdown.store(false, Ordering::Release);
        while slot.queue.pop().is_some() {}
        let ring = Arc::clone(&slot.queue);
        let shutdown = Arc::clone(&slot.shutdown);
        let failure_tx = self.inner.mid_stream_tx.clone();
        let track_id_for_panic = track_id.clone();
        let failure_tx_for_panic = failure_tx.clone();
        std::thread::Builder::new()
            .name(format!("hh-decode-test-{track_id}"))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    body(ring, shutdown, failure_tx, track_id);
                }));
                if let Err(payload) = result {
                    let msg = panic_payload_to_string(&payload);
                    try_send_mid_stream_failure(
                        &failure_tx_for_panic,
                        MidStreamFailure {
                            track_id: track_id_for_panic,
                            deck: None,
                            kind: MidStreamFailureKind::ThreadPanic(msg),
                        },
                    );
                }
            })
            .expect("failed to spawn test decoder thread")
    }
}

// ---------------------------------------------------------------------------
// Handle encoding
// ---------------------------------------------------------------------------

#[inline]
fn encode_handle(slot_idx: usize, id: u32) -> u32 {
    // Top 4 bits = slot_idx (0..16); bottom 28 bits = id (per-handle
    // generation counter so reuse of a slot doesn't collide if a stale
    // handle leaks). Internally only slot_idx matters; id is logged.
    debug_assert!(slot_idx < MAX_DECODE_SLOTS);
    ((slot_idx as u32) << 28) | (id & 0x0FFF_FFFF)
}

#[inline]
fn decode_handle(h: DecodeHandle) -> (usize, u32) {
    let slot = (h.0 >> 28) as usize;
    let id = h.0 & 0x0FFF_FFFF;
    (slot, id)
}

// ---------------------------------------------------------------------------
// Decoder thread
// ---------------------------------------------------------------------------

fn open_media_source(
    path: &str,
    inline: &Mutex<HashMap<String, Vec<u8>>>,
) -> Result<MediaSourceStream, DecodeError> {
    if let Some(key) = path.strip_prefix(MEM_PREFIX) {
        let guard = inline.lock().expect("inline_sources poisoned");
        let bytes = guard
            .get(key)
            .cloned()
            .ok_or_else(|| DecodeError::UnknownInlineSource(key.to_string()))?;
        let cursor = Cursor::new(bytes);
        return Ok(MediaSourceStream::new(Box::new(cursor), Default::default()));
    }
    // HTTP(S) URLs are handled by the streaming media source — used
    // by SoundCloud-style sources landed in PR #107 (closes #106).
    // The decoder thread (per-track, on its own OS thread) is the
    // only consumer of this source, so blocking network I/O here is
    // safe re: ADR-004 (audio thread stays untouched).
    if path.starts_with("http://") || path.starts_with("https://") {
        let src = crate::audio::http_source::HttpMediaSource::open(path).map_err(|e| {
            DecodeError::Io {
                path: path.to_string(),
                source: e,
            }
        })?;
        return Ok(MediaSourceStream::new(Box::new(src), Default::default()));
    }
    let file = File::open(Path::new(path)).map_err(|e| DecodeError::Io {
        path: path.to_string(),
        source: e,
    })?;
    Ok(MediaSourceStream::new(Box::new(file), Default::default()))
}

/// Top-level entry point for a per-track decoder thread. Wraps the
/// real decode body in `catch_unwind` so a panic inside symphonia /
/// rubato / our own code cannot bring down the audio system —
/// instead the panic surfaces as a `decoder_thread_panic` notification
/// to connected UI clients and the audio thread continues to
/// silence-pad the now-quiet ring.
///
/// No `unsafe` is used. `AssertUnwindSafe` is applied to the closure
/// because the local `Vec` scratch buffers + the resampler are not
/// `RefUnwindSafe`; we accept the unsoundness risk in exchange for
/// not crashing the whole engine, which matches the std-lib pattern
/// for "boundary" threads.
fn decoder_thread_main(
    mss: MediaSourceStream,
    target_sr: u32,
    ring: Arc<ArrayQueue<f32>>,
    shutdown: Arc<AtomicBool>,
    failure_tx: Sender<MidStreamFailure>,
    track_id: String,
) {
    let track_id_for_panic = track_id.clone();
    let failure_tx_for_panic = failure_tx.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        decoder_thread_inner(mss, target_sr, &ring, &shutdown, &failure_tx, &track_id)
    }));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(target: "decode", error = ?e, "decoder thread exited with error");
        }
        Err(panic_payload) => {
            let msg = panic_payload_to_string(&panic_payload);
            tracing::error!(
                target: "decode",
                track_id = %track_id_for_panic,
                panic = %msg,
                "decoder thread panicked — surfacing as decoder_thread_panic",
            );
            try_send_mid_stream_failure(
                &failure_tx_for_panic,
                MidStreamFailure {
                    track_id: track_id_for_panic,
                    deck: None,
                    kind: MidStreamFailureKind::ThreadPanic(msg),
                },
            );
        }
    }
}

/// Best-effort coerce a `Box<dyn Any + Send>` from `catch_unwind` to
/// a readable string. Covers the two common payload shapes the std
/// library docs guarantee — `&'static str` and `String` — and falls
/// back to a placeholder for unknown payloads so the UI always sees
/// non-empty `error` text.
fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "decoder thread panicked (non-string payload)".to_string()
}

/// Non-blocking push onto the failure sidechannel. The decoder thread
/// must never block on a slow consumer, so a full channel (= 64
/// queued failures the bridge hasn't drained yet) silently drops the
/// event after a `warn!`. Channel-closed is also dropped (bridge gone).
fn try_send_mid_stream_failure(tx: &Sender<MidStreamFailure>, failure: MidStreamFailure) {
    match tx.try_send(failure) {
        Ok(()) => {}
        Err(TrySendError::Full(dropped)) => {
            tracing::warn!(
                target: "decode",
                track_id = %dropped.track_id,
                category = %dropped.kind.category(),
                "mid-stream failure channel full — dropping event",
            );
        }
        Err(TrySendError::Disconnected(dropped)) => {
            tracing::debug!(
                target: "decode",
                track_id = %dropped.track_id,
                "mid-stream failure channel closed — bridge shut down",
            );
        }
    }
}

fn decoder_thread_inner(
    mss: MediaSourceStream,
    target_sr: u32,
    ring: &Arc<ArrayQueue<f32>>,
    shutdown: &Arc<AtomicBool>,
    failure_tx: &Sender<MidStreamFailure>,
    track_id_label: &str,
) -> Result<(), DecodeError> {
    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| DecodeError::Probe(format!("{e}")))?;
    let mut reader = probed.format;
    let track = reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(DecodeError::NoTrack)?;
    let track_id = track.id;
    let source_sr = track.codec_params.sample_rate.unwrap_or(target_sr);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| DecodeError::Probe(format!("codec init: {e}")))?;

    // Per-track resampler — `None` means passthrough.
    let mut resampler: Option<SincFixedIn<f32>> = if source_sr != target_sr {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        let r = SincFixedIn::<f32>::new(
            target_sr as f64 / source_sr as f64,
            1.1, // max relative ratio
            params,
            1024,
            2,
        )
        .map_err(|e| DecodeError::Resampler(format!("{e}")))?;
        Some(r)
    } else {
        None
    };

    // Reusable scratch buffers (decoder thread can allocate freely).
    let mut in_left: Vec<f32> = Vec::with_capacity(8192);
    let mut in_right: Vec<f32> = Vec::with_capacity(8192);

    while !shutdown.load(Ordering::Acquire) {
        let packet = match reader.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => {
                // Mid-stream read failure (corrupt frame, truncated
                // file, unexpected EOF outside the recoverable variant
                // above). Pre-PR-#56-followup this exited silently —
                // now we surface it as `mid_stream_decode_failure`.
                let msg = format!("reader.next_packet failed: {e}");
                tracing::warn!(target: "decode", error = ?e, "{msg}");
                try_send_mid_stream_failure(
                    failure_tx,
                    MidStreamFailure {
                        track_id: track_id_label.to_string(),
                        deck: None,
                        kind: MidStreamFailureKind::DecodeFailed(msg),
                    },
                );
                break;
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // `DecodeError` is symphonia's recoverable-corruption
            // variant — the standard advice is to skip the packet and
            // keep going. We DON'T surface these as mid-stream
            // failures (would spam the UI on glitchy MP3s).
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => {
                let msg = format!("decoder.decode failed: {e}");
                tracing::warn!(target: "decode", error = ?e, "{msg}");
                try_send_mid_stream_failure(
                    failure_tx,
                    MidStreamFailure {
                        track_id: track_id_label.to_string(),
                        deck: None,
                        kind: MidStreamFailureKind::DecodeFailed(msg),
                    },
                );
                break;
            }
        };

        in_left.clear();
        in_right.clear();
        extract_stereo(decoded, &mut in_left, &mut in_right);

        if let Err(e) = push_into_ring(&in_left, &in_right, resampler.as_mut(), ring, shutdown) {
            let msg = format!("rubato resample failed mid-stream: {e}");
            tracing::warn!(target: "decode", error = %e, "{msg}");
            try_send_mid_stream_failure(
                failure_tx,
                MidStreamFailure {
                    track_id: track_id_label.to_string(),
                    deck: None,
                    kind: MidStreamFailureKind::ResampleFailed(msg),
                },
            );
            break;
        }
    }
    Ok(())
}

/// Copy any AudioBufferRef into two f32 planar channels. Mono sources
/// duplicate to both channels; >2-channel sources keep L=ch0, R=ch1.
fn extract_stereo(buf: AudioBufferRef<'_>, left: &mut Vec<f32>, right: &mut Vec<f32>) {
    match buf {
        AudioBufferRef::F32(b) => fill_stereo(&b, left, right, |s| *s),
        AudioBufferRef::F64(b) => fill_stereo(&b, left, right, |s| *s as f32),
        AudioBufferRef::S8(b) => {
            let scale = 1.0_f32 / i8::MAX as f32;
            fill_stereo(&b, left, right, |s| *s as f32 * scale)
        }
        AudioBufferRef::S16(b) => {
            let scale = 1.0_f32 / i16::MAX as f32;
            fill_stereo(&b, left, right, |s| *s as f32 * scale)
        }
        AudioBufferRef::S24(b) => {
            let scale = 1.0_f32 / (1 << 23) as f32;
            fill_stereo(&b, left, right, |s| s.inner() as f32 * scale)
        }
        AudioBufferRef::S32(b) => {
            let scale = 1.0_f32 / i32::MAX as f32;
            fill_stereo(&b, left, right, |s| *s as f32 * scale)
        }
        AudioBufferRef::U8(b) => fill_stereo(&b, left, right, |s| (*s as f32 - 128.0) / 128.0),
        AudioBufferRef::U16(b) => fill_stereo(&b, left, right, |s| (*s as f32 - 32768.0) / 32768.0),
        AudioBufferRef::U24(b) => {
            let scale = 1.0_f32 / (1 << 23) as f32;
            fill_stereo(&b, left, right, |s| {
                (s.inner() as f32 - (1 << 23) as f32) * scale
            })
        }
        AudioBufferRef::U32(b) => {
            let half = (u32::MAX / 2) as f32;
            fill_stereo(&b, left, right, |s| (*s as f32 - half) / half)
        }
    }
}

fn fill_stereo<S, F>(
    b: &symphonia::core::audio::AudioBuffer<S>,
    left: &mut Vec<f32>,
    right: &mut Vec<f32>,
    conv: F,
) where
    S: symphonia::core::sample::Sample,
    F: Fn(&S) -> f32,
{
    use symphonia::core::audio::Signal;
    let frames = b.frames();
    let chans = b.spec().channels.count();
    match chans {
        0 => {}
        1 => {
            let l = b.chan(0);
            for sample in l.iter().take(frames) {
                let s = conv(sample);
                left.push(s);
                right.push(s);
            }
        }
        _ => {
            let l = b.chan(0);
            let r = b.chan(1);
            for (ls, rs) in l.iter().take(frames).zip(r.iter().take(frames)) {
                left.push(conv(ls));
                right.push(conv(rs));
            }
        }
    }
}

/// Resample (if needed), interleave, and push into the ring. If the
/// ring is full, the decoder thread spins with a tiny sleep — pushing
/// is throttled by the audio thread's consumption rate.
///
/// Returns `Err` only on rubato resample failure (non-finite input,
/// internal state corruption); the caller surfaces this as a
/// `mid_stream_decode_failure` notification. The pre-existing
/// "break-on-error" behaviour is preserved via `?` from the caller
/// (same effect: decoder thread exits cleanly).
fn push_into_ring(
    left: &[f32],
    right: &[f32],
    resampler: Option<&mut SincFixedIn<f32>>,
    ring: &Arc<ArrayQueue<f32>>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), String> {
    if left.is_empty() {
        return Ok(());
    }
    let (out_l, out_r): (Vec<f32>, Vec<f32>) = match resampler {
        None => (left.to_vec(), right.to_vec()),
        Some(r) => {
            let want = r.input_frames_next();
            let mut acc_l = Vec::with_capacity(left.len() * 2);
            let mut acc_r = Vec::with_capacity(right.len() * 2);
            let mut pos = 0usize;
            while pos < left.len() && !shutdown.load(Ordering::Acquire) {
                let end = (pos + want).min(left.len());
                let mut chunk_l = left[pos..end].to_vec();
                let mut chunk_r = right[pos..end].to_vec();
                if chunk_l.len() < want {
                    chunk_l.resize(want, 0.0);
                    chunk_r.resize(want, 0.0);
                }
                let out = match r.process(&[&chunk_l, &chunk_r], None) {
                    Ok(o) => o,
                    Err(e) => return Err(format!("{e}")),
                };
                acc_l.extend_from_slice(&out[0]);
                acc_r.extend_from_slice(&out[1]);
                pos += want;
            }
            (acc_l, acc_r)
        }
    };
    for (l, r) in out_l.iter().zip(out_r.iter()) {
        push_blocking(ring, *l, shutdown);
        push_blocking(ring, *r, shutdown);
    }
    Ok(())
}

#[inline]
fn push_blocking(ring: &Arc<ArrayQueue<f32>>, sample: f32, shutdown: &Arc<AtomicBool>) {
    let mut s = sample;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match ring.push(s) {
            Ok(()) => return,
            Err(returned) => {
                s = returned;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// StubDecodeService — synthesises a sine wave; alloc-free `read`.
// ---------------------------------------------------------------------------

/// Test/dev stub that returns a 440 Hz stereo sine for any track. Used
/// by translator unit tests + by integration tests that don't want to
/// depend on symphonia decode.
///
/// Per-handle state lives in a fixed-size array indexed by handle id so
/// `read` is alloc-free. Bench / hot-path tests should still prefer the
/// stub over the symphonia service.
pub struct StubDecodeService {
    inner: Arc<StubShared>,
}

struct StubShared {
    next_id: AtomicU64,
    underruns: AtomicU64,
    // Per-slot phase + sample rate, packed as bit-shifted u64 atomics.
    // Top 32 bits = sample_rate; bottom 32 bits = bit-pattern of phase f32.
    // Audio thread reads + writes via Relaxed atomics — alloc-free + lock-free.
    slots: Vec<AtomicU64>,
    occupied: Vec<AtomicBool>,
}

impl Default for StubDecodeService {
    fn default() -> Self {
        Self::new()
    }
}

impl StubDecodeService {
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(MAX_DECODE_SLOTS);
        let mut occupied = Vec::with_capacity(MAX_DECODE_SLOTS);
        for _ in 0..MAX_DECODE_SLOTS {
            slots.push(AtomicU64::new(pack_state(48_000, 0.0)));
            occupied.push(AtomicBool::new(false));
        }
        Self {
            inner: Arc::new(StubShared {
                next_id: AtomicU64::new(1),
                underruns: AtomicU64::new(0),
                slots,
                occupied,
            }),
        }
    }
}

impl Clone for StubDecodeService {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[inline]
fn pack_state(sample_rate: u32, phase: f32) -> u64 {
    ((sample_rate as u64) << 32) | (phase.to_bits() as u64)
}

#[inline]
fn unpack_state(v: u64) -> (u32, f32) {
    let sr = (v >> 32) as u32;
    let phase = f32::from_bits((v & 0xFFFF_FFFF) as u32);
    (sr, phase)
}

impl DecodeService for StubDecodeService {
    fn open(
        &self,
        _track: &TrackRef,
        target_sample_rate: u32,
    ) -> Result<DecodeHandle, DecodeError> {
        let slot_idx = self
            .inner
            .occupied
            .iter()
            .position(|o| {
                o.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            })
            .ok_or(DecodeError::NoFreeSlot)?;
        self.inner.slots[slot_idx].store(pack_state(target_sample_rate, 0.0), Ordering::Release);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) as u32;
        Ok(DecodeHandle(encode_handle(slot_idx, id)))
    }

    fn read(&self, handle: DecodeHandle, buf: &mut [f32]) -> usize {
        if !handle.is_some() || buf.is_empty() {
            return 0;
        }
        let (slot_idx, _id) = decode_handle(handle);
        if slot_idx >= self.inner.slots.len() {
            return 0;
        }
        if !self.inner.occupied[slot_idx].load(Ordering::Acquire) {
            return 0;
        }
        // Generate a 440 Hz sine on both channels. Phase is held as an
        // atomic so concurrent readers don't desync; in the engine's
        // single-audio-thread model there's no real contention.
        let (sr, mut phase) = unpack_state(self.inner.slots[slot_idx].load(Ordering::Relaxed));
        let dphase = std::f32::consts::TAU * 440.0 / sr.max(1) as f32;
        for chunk in buf.chunks_mut(2) {
            let s = phase.sin() * 0.2;
            for slot in chunk.iter_mut() {
                *slot = s;
            }
            phase += dphase;
            if phase > std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
        }
        self.inner.slots[slot_idx].store(pack_state(sr, phase), Ordering::Relaxed);
        buf.len()
    }

    fn close(&self, handle: DecodeHandle) {
        if !handle.is_some() {
            return;
        }
        let (slot_idx, _id) = decode_handle(handle);
        if slot_idx >= self.inner.slots.len() {
            return;
        }
        self.inner.occupied[slot_idx].store(false, Ordering::Release);
    }

    fn underrun_count(&self) -> u64 {
        self.inner.underruns.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    fn track(id: &str, path: &str) -> TrackRef {
        TrackRef {
            id: id.to_string(),
            path: path.to_string(),
        }
    }

    /// Build a tiny synthetic WAV file in memory: RIFF + fmt + data
    /// chunks. PCM 16-bit, given channels + sample rate, `samples` is
    /// already interleaved.
    pub(crate) fn build_wav(channels: u16, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let bits_per_sample = 16u16;
        let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
        let block_align = channels * bits_per_sample / 8;
        let data_bytes = (samples.len() * 2) as u32;
        let mut v = Vec::with_capacity(44 + samples.len() * 2);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits_per_sample.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_bytes.to_le_bytes());
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    pub(crate) fn sine_pcm16(freq: f32, sr: u32, secs: f32, channels: u16) -> Vec<i16> {
        let n = (sr as f32 * secs) as usize;
        let mut out = Vec::with_capacity(n * channels as usize);
        let tau = std::f32::consts::TAU;
        for i in 0..n {
            let s = (tau * freq * (i as f32 / sr as f32)).sin();
            let v = (s * 0.5 * i16::MAX as f32) as i16;
            for _ in 0..channels {
                out.push(v);
            }
        }
        out
    }

    // --- SymphoniaDecodeService -------------------------------------

    #[test]
    fn handle_encoding_round_trip() {
        for slot in 0..MAX_DECODE_SLOTS {
            for id in [1u32, 7, 999, 0x0FFF_FFFE] {
                let h = DecodeHandle(encode_handle(slot, id));
                let (s2, i2) = decode_handle(h);
                assert_eq!(s2, slot);
                assert_eq!(i2, id);
            }
        }
    }

    #[test]
    fn underrun_fills_zero_and_increments_counter() {
        let svc = SymphoniaDecodeService::new();
        // ~10 ms of input, then we'll over-drain.
        let wav = build_wav(1, 48_000, &sine_pcm16(440.0, 48_000, 0.01, 1));
        svc.register_inline_source("u", wav);
        let h = svc.open(&track("u", "mem://u"), 48_000).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let mut buf = [0.0_f32; 16384];
        // Drain enough to definitely underrun.
        for _ in 0..6 {
            let _ = svc.read(h, &mut buf);
        }
        assert!(
            svc.underrun_count() >= 1,
            "expected underrun events, got {}",
            svc.underrun_count()
        );
        // Last samples should be silence after underrun.
        assert!(buf[buf.len() - 1].abs() < 1e-6);
        svc.close(h);
    }

    #[test]
    fn mono_input_duplicates_to_stereo() {
        let svc = SymphoniaDecodeService::new();
        let wav = build_wav(1, 48_000, &sine_pcm16(1_000.0, 48_000, 0.2, 1));
        svc.register_inline_source("mono", wav);
        let h = svc.open(&track("mono", "mem://mono"), 48_000).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        let mut buf = [0.0_f32; 4096];
        let _ = svc.read(h, &mut buf);
        let mut mismatches = 0;
        for i in (0..buf.len()).step_by(2) {
            if (buf[i] - buf[i + 1]).abs() > 1e-6 {
                mismatches += 1;
            }
        }
        assert_eq!(mismatches, 0, "mono input must duplicate L=R");
        assert!(
            buf.iter().any(|s| s.abs() > 0.05),
            "expected non-trivial audio energy"
        );
        svc.close(h);
    }

    #[test]
    fn rate_conversion_22050_to_48000_passes_audio() {
        let svc = SymphoniaDecodeService::new();
        let wav = build_wav(1, 22_050, &sine_pcm16(440.0, 22_050, 1.0, 1));
        svc.register_inline_source("rate", wav);
        let h = svc.open(&track("rate", "mem://rate"), 48_000).unwrap();
        // Give rubato + decoder enough wall time for at least a few
        // 1024-frame chunks to traverse the pipeline.
        std::thread::sleep(Duration::from_millis(900));
        let mut buf = [0.0_f32; 24_000]; // 12k stereo frames
        let _ = svc.read(h, &mut buf);
        let energy: f32 = buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32;
        assert!(energy > 1e-4, "resampled signal energy too low: {energy}");
        svc.close(h);
    }

    #[test]
    fn close_stops_decoder_thread() {
        let svc = SymphoniaDecodeService::new();
        let wav = build_wav(2, 48_000, &sine_pcm16(440.0, 48_000, 5.0, 2));
        svc.register_inline_source("long", wav);
        let h = svc.open(&track("long", "mem://long"), 48_000).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(svc.open_slot_count(), 1);
        svc.close(h);
        assert_eq!(svc.open_slot_count(), 0);
        // Idempotent close.
        svc.close(h);
        assert_eq!(svc.open_slot_count(), 0);
    }

    #[test]
    fn open_close_reuses_slots() {
        let svc = SymphoniaDecodeService::new();
        for i in 0..(MAX_DECODE_SLOTS * 2) {
            let key = format!("loop-{i}");
            let wav = build_wav(1, 48_000, &sine_pcm16(440.0, 48_000, 0.05, 1));
            svc.register_inline_source(&key, wav);
            let h = svc
                .open(&track(&key, &format!("mem://{key}")), 48_000)
                .unwrap();
            std::thread::sleep(Duration::from_millis(20));
            svc.close(h);
        }
        assert_eq!(svc.open_slot_count(), 0);
    }

    #[test]
    fn slot_exhaustion_returns_no_free_slot() {
        let svc = SymphoniaDecodeService::new();
        let mut handles = Vec::new();
        for i in 0..MAX_DECODE_SLOTS {
            let key = format!("ex-{i}");
            let wav = build_wav(1, 48_000, &sine_pcm16(440.0, 48_000, 0.5, 1));
            svc.register_inline_source(&key, wav);
            let h = svc
                .open(&track(&key, &format!("mem://{key}")), 48_000)
                .unwrap();
            handles.push(h);
        }
        let key = "ex-overflow";
        let wav = build_wav(1, 48_000, &sine_pcm16(440.0, 48_000, 0.1, 1));
        svc.register_inline_source(key, wav);
        let res = svc.open(&track(key, &format!("mem://{key}")), 48_000);
        assert!(matches!(res, Err(DecodeError::NoFreeSlot)));
        for h in handles {
            svc.close(h);
        }
    }

    #[test]
    fn read_on_none_handle_returns_zero() {
        let svc = SymphoniaDecodeService::new();
        let mut buf = [0.5_f32; 64];
        let n = svc.read(DecodeHandle::NONE, &mut buf);
        assert_eq!(n, 0);
        assert!(buf.iter().all(|s| (*s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn read_on_unknown_file_path_errors_at_open() {
        let svc = SymphoniaDecodeService::new();
        let res = svc.open(&track("ghost", "/nonexistent/file.wav"), 48_000);
        assert!(
            matches!(res, Err(DecodeError::Io { .. })),
            "expected IO error, got {res:?}"
        );
        // Slot must be released back to the pool on error.
        assert_eq!(svc.open_slot_count(), 0);
    }

    // --- Stub service ------------------------------------------------

    #[test]
    fn stub_open_returns_distinct_handles() {
        let svc = StubDecodeService::new();
        let a = svc.open(&track("a", "/a.mp3"), 48_000).unwrap();
        let b = svc.open(&track("b", "/b.mp3"), 48_000).unwrap();
        assert_ne!(a, b);
        svc.close(a);
        svc.close(b);
    }

    #[test]
    fn stub_read_generates_nonzero_stereo() {
        let svc = StubDecodeService::new();
        let h = svc.open(&track("t", "/t.mp3"), 48_000).unwrap();
        let mut buf = [0.0_f32; 256];
        let n = svc.read(h, &mut buf);
        assert_eq!(n, buf.len());
        let energy: f32 = buf.iter().map(|s| s * s).sum();
        assert!(energy > 0.0);
        for i in (0..buf.len()).step_by(2) {
            assert!((buf[i] - buf[i + 1]).abs() < 1e-6);
        }
        svc.close(h);
    }

    #[test]
    fn stub_read_is_alloc_free() {
        let svc = StubDecodeService::new();
        let h = svc.open(&track("t", "/t.mp3"), 48_000).unwrap();
        let mut buf = [0.0_f32; 1024];
        assert_no_alloc::assert_no_alloc(|| {
            let _ = svc.read(h, &mut buf);
        });
        svc.close(h);
    }

    // --- Mid-stream failure sidechannel (PR #56 follow-up) -----------

    /// Helper: drain the sidechannel with a deadline; returns the
    /// first failure observed or `None` on timeout.
    fn recv_failure_within(
        rx: &Receiver<MidStreamFailure>,
        deadline: Duration,
    ) -> Option<MidStreamFailure> {
        rx.recv_timeout(deadline).ok()
    }

    #[test]
    fn mid_stream_failure_kind_category_mapping() {
        assert_eq!(
            MidStreamFailureKind::DecodeFailed("x".into()).category(),
            "mid_stream_decode_failure",
        );
        assert_eq!(
            MidStreamFailureKind::ResampleFailed("x".into()).category(),
            "mid_stream_decode_failure",
        );
        assert_eq!(
            MidStreamFailureKind::ThreadPanic("x".into()).category(),
            "decoder_thread_panic",
        );
    }

    #[test]
    fn take_mid_stream_failure_receiver_hands_out_once() {
        let svc = SymphoniaDecodeService::new();
        assert!(svc.take_mid_stream_failure_receiver().is_some());
        // Second take returns None — single-consumer contract.
        assert!(svc.take_mid_stream_failure_receiver().is_none());
    }

    #[test]
    fn stub_service_returns_no_sidechannel_by_default() {
        // The default trait impl returns None — confirms `StubDecodeService`
        // (which doesn't override) opts out of the sidechannel cleanly.
        let svc = StubDecodeService::new();
        assert!(svc.take_mid_stream_failure_receiver().is_none());
    }

    #[test]
    fn injected_mid_stream_decode_failure_reaches_receiver() {
        // Models the "synthetic decoder fails after N samples" path:
        // we exercise the sidechannel directly so the test doesn't
        // depend on coaxing a specific symphonia error mid-track.
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        svc.__inject_mid_stream_failure_for_test(MidStreamFailure {
            track_id: "trk-mid-1".into(),
            deck: Some('A'),
            kind: MidStreamFailureKind::DecodeFailed("synthetic bitstream corruption".into()),
        });
        let got = recv_failure_within(&rx, Duration::from_millis(200))
            .expect("mid-stream failure should reach receiver");
        assert_eq!(got.track_id, "trk-mid-1");
        assert_eq!(got.deck, Some('A'));
        assert_eq!(got.kind.category(), "mid_stream_decode_failure");
        assert!(got.kind.message().contains("bitstream corruption"));
    }

    /// Skip catch_unwind+sidechannel tests on Windows when running on a
    /// shared GitHub-hosted CI runner. Under that scheduler the crossbeam
    /// channel `Sender::send` race vs `JoinHandle::join` can drop the
    /// failure event before the receiver wakes (observed on PR #98/#100).
    /// On any other runner — local Windows dev box, self-hosted GH
    /// runner, Linux + macOS — the tests execute normally.
    ///
    /// Set `HYPEHOUSE_SHARED_CI_RUNNER=1` on the runner to suppress;
    /// unset / `0` runs the tests. Tracked by issue #110.
    fn windows_shared_ci_runner() -> bool {
        cfg!(target_os = "windows")
            && std::env::var("HYPEHOUSE_SHARED_CI_RUNNER").ok().as_deref() == Some("1")
    }

    // Windows shared CI: catch_unwind + sidechannel send timing flakes on
    // GitHub-hosted runners (#110). Local Windows + self-hosted runners
    // still execute. Linux + macOS always execute.
    #[test]
    fn panicking_test_decoder_thread_surfaces_decoder_thread_panic() {
        if windows_shared_ci_runner() {
            eprintln!(
                "skipping panicking_test_decoder_thread_surfaces_decoder_thread_panic on \
                 Windows shared CI runner (HYPEHOUSE_SHARED_CI_RUNNER=1); see #110"
            );
            return;
        }
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        // Spawn a "decoder" that just panics — exercises the
        // catch_unwind guard in the test-spawn helper which mirrors
        // the production path.
        let join =
            svc.__spawn_test_decoder("trk-panic-1".to_string(), |_ring, _shutdown, _tx, _id| {
                panic!("synthetic panic in decoder thread");
            });
        let got = recv_failure_within(&rx, Duration::from_millis(500))
            .expect("panic should surface as a sidechannel event");
        assert_eq!(got.track_id, "trk-panic-1");
        assert_eq!(got.kind.category(), "decoder_thread_panic");
        assert!(
            got.kind.message().contains("synthetic panic"),
            "message should preserve panic payload: {}",
            got.kind.message()
        );
        // The thread itself should be joinable (it caught the unwind).
        join.join().expect("test decoder thread joined");
    }

    #[test]
    fn audio_thread_continues_silence_pad_after_decoder_panic() {
        if windows_shared_ci_runner() {
            eprintln!(
                "skipping audio_thread_continues_silence_pad_after_decoder_panic on \
                 Windows shared CI runner (HYPEHOUSE_SHARED_CI_RUNNER=1); see #110"
            );
            return;
        }
        // The whole point of catch_unwind on the decoder thread is
        // that the audio thread keeps running — `read()` must still
        // return cleanly (silence) after the decoder panics and
        // posts its failure event.
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        let join =
            svc.__spawn_test_decoder("trk-panic-2".to_string(), |_ring, _shutdown, _tx, _id| {
                // Decoder panics before pushing anything onto the ring.
                panic!("decoder thread panic — ring stays empty");
            });
        // Drain the sidechannel so the test is deterministic.
        let _ = recv_failure_within(&rx, Duration::from_millis(500))
            .expect("panic surfaced as sidechannel event");
        join.join().expect("test decoder thread joined");
        // Audio-side read: must NOT panic, must return silence for
        // every sample (the ring is empty, slot is still occupied).
        // We sweep every slot because `__spawn_test_decoder` claims
        // the first free one — handle math is internal to the helper.
        let mut buf = [1.0_f32; 32];
        for slot_idx in 0..MAX_DECODE_SLOTS {
            let h = DecodeHandle(encode_handle(slot_idx, 1));
            let _ = svc.read(h, &mut buf);
        }
        // At least one slot should have written silence over the
        // sentinel `1.0` values (the one our test decoder claimed).
        assert!(
            buf.contains(&0.0),
            "audio thread should have silence-padded after panic",
        );
    }

    #[test]
    fn sidechannel_full_drops_event_without_blocking_decoder_thread() {
        // Backpressure contract: a full sidechannel must NOT block the
        // decoder thread (would freeze the audio pipeline). Instead
        // events drop with a warn. We saturate the channel + verify
        // a subsequent push completes promptly.
        let svc = SymphoniaDecodeService::new();
        let _rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        for i in 0..MID_STREAM_FAILURE_CAPACITY {
            svc.__inject_mid_stream_failure_for_test(MidStreamFailure {
                track_id: format!("flood-{i}"),
                deck: None,
                kind: MidStreamFailureKind::DecodeFailed("flood".into()),
            });
        }
        // One more push past capacity — must return immediately, not
        // block.
        let start = std::time::Instant::now();
        svc.__inject_mid_stream_failure_for_test(MidStreamFailure {
            track_id: "overflow".into(),
            deck: None,
            kind: MidStreamFailureKind::DecodeFailed("dropped".into()),
        });
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "try_send on full channel must not block; took {elapsed:?}",
        );
    }

    #[test]
    fn synthetic_decoder_fails_after_n_samples_surfaces_mid_stream_failure() {
        // Models the explicit task requirement: "synthetic decoder
        // that fails after N samples → mid_stream_decode_failure".
        // The test decoder pushes a handful of samples then posts a
        // mid-stream failure exactly the same way the production
        // decoder thread does (via `try_send_mid_stream_failure`).
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        const N: usize = 256;
        let join = svc.__spawn_test_decoder(
            "trk-mid-N".to_string(),
            move |ring, _shutdown, tx, track_id| {
                // Push N samples of silence, then fail.
                for _ in 0..N {
                    let _ = ring.push(0.0);
                }
                try_send_mid_stream_failure(
                    &tx,
                    MidStreamFailure {
                        track_id,
                        deck: Some('B'),
                        kind: MidStreamFailureKind::DecodeFailed(
                            "synthetic packet corruption after N samples".into(),
                        ),
                    },
                );
            },
        );
        join.join().expect("test decoder thread joined");
        let got = recv_failure_within(&rx, Duration::from_millis(500))
            .expect("mid-stream failure should surface");
        assert_eq!(got.track_id, "trk-mid-N");
        assert_eq!(got.deck, Some('B'));
        assert_eq!(got.kind.category(), "mid_stream_decode_failure");
        assert!(got.kind.message().contains("after N samples"));
    }

    #[test]
    fn open_time_error_path_still_returns_err_and_no_sidechannel_event() {
        // Regression guard for PR #56: open-time errors must STILL
        // bubble up synchronously as `DecodeError` (no behaviour
        // change) and MUST NOT spuriously post a mid-stream event.
        let svc = SymphoniaDecodeService::new();
        let rx = svc
            .take_mid_stream_failure_receiver()
            .expect("rx claimable");
        let res = svc.open(&track("ghost", "/nonexistent/at/open.wav"), 48_000);
        assert!(matches!(res, Err(DecodeError::Io { .. })));
        // No mid-stream event should appear within a short grace
        // window (the open failure is a pre-spawn synchronous path).
        assert!(
            recv_failure_within(&rx, Duration::from_millis(50)).is_none(),
            "open-time errors must not produce mid-stream sidechannel events",
        );
    }
}
