//! Per-session master-mix recorder (WAV).
//!
//! The recorder tees the engine's final mixed audio to a WAV file so a
//! DJ can listen back to (or share / edit) their set after a session.
//! It lives alongside the [`crate::audio::AudioMixer`] in the audio
//! thread but does **zero** blocking I/O there — the audio side only
//! pushes interleaved-stereo `f32` samples into a lock-free SPSC ring
//! and increments an atomic drop counter when the ring is momentarily
//! full. A dedicated writer thread drains the ring and writes the WAV
//! file. On [`MasterRecorder::stop`] the writer is joined and the
//! header is patched with the final data size.
//!
//! # Architecture
//!
//! ```text
//!     audio thread                  writer thread
//!   ┌──────────────┐   ringbuf    ┌────────────────┐
//!   │ AudioMixer   │ ──f32 SPSC──▶│ drain → BufWrt │ ──▶ master.wav
//!   │  .render()   │              │ (WAV body)     │
//!   │  → sink.push │              └────────────────┘
//!   └──────────────┘                       │
//!         ▲                                │ on stop:
//!         │                                │   seek 4   → file_size-8
//!         └── drops counter ◀── atomic ────┘   seek 40  → data_size
//!                                              fsync
//! ```
//!
//! # WAV format (in-house, ~30 lines)
//!
//! * PCM IEEE float (format = 3), 32-bit, stereo, sample rate from the
//!   audio device.
//! * Header is 44 bytes; placeholder data length is patched in
//!   [`MasterRecorder::stop`] once the final byte count is known.
//! * No `hound` dependency — the format is trivial and keeping the
//!   surface small reduces supply-chain risk.
//!
//! # ADR-004 compliance (audio thread)
//!
//! The audio thread's push path is a single `push_slice` into a
//! pre-allocated `HeapProd<f32>` and one `fetch_add` on a counter when
//! the slice didn't fit. No allocation, no syscalls, no locks. Verified
//! in `tests::push_is_alloc_free`.
//!
//! # Env
//!
//! * `HYPEHOUSE_RECORDING_DISABLED=1` — skip creating the recorder.
//!   Used by tests + ephemeral runs.

use anyhow::{Context, Result};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Env var that disables recording entirely.
pub const ENV_RECORDING_DISABLED: &str = "HYPEHOUSE_RECORDING_DISABLED";

/// Capacity of the SPSC ring in **f32 samples** (not frames).
///
/// 1 second @ 48 kHz stereo = 96 000 samples. At 32-bit float that is
/// 384 KiB — small enough to fit in L2 on every target host, large
/// enough that a writer-thread stall (page-cache miss, disk hiccup) of
/// up to ~1 s is absorbed without dropping frames. The writer thread
/// drains at a 10 ms cadence so steady-state occupancy is near zero.
pub const RING_CAPACITY_SAMPLES: usize = 96_000;

/// WAV header size in bytes for our fixed-shape (PCM IEEE-float
/// stereo) layout. Documented inline in [`write_wav_header`].
pub const WAV_HEADER_BYTES: u64 = 44;

/// Offset of the `RIFF` size field (a `u32` LE that will be patched to
/// `file_size - 8` on stop).
const OFF_RIFF_SIZE: u64 = 4;

/// Offset of the `data` chunk size field (a `u32` LE patched to the
/// final PCM body size on stop).
const OFF_DATA_SIZE: u64 = 40;

/// Writer-thread drain cadence. Trade-off: shorter = lower latency
/// before bytes hit the page cache; longer = fewer syscalls. 10 ms is
/// comfortably under the human "did I just record this?" threshold and
/// drains a fully-saturated audio thread @ 48 kHz stereo in ~1 ms of
/// CPU.
const WRITER_DRAIN_INTERVAL: Duration = Duration::from_millis(10);

/// Audio-thread side of the recorder. Owned by [`AudioMixer`] (or any
/// hot-path producer). Push path is alloc-free + wait-free.
///
/// Constructed via [`MasterRecorder::split`]. When the writer side is
/// dropped (e.g. via [`MasterRecorder::stop`]) the producer can still
/// be pushed to safely — the writer just no longer drains, so the ring
/// fills and overflow is reported via [`MasterRecorderSink::dropped_frames`].
pub struct MasterRecorderSink {
    producer: HeapProd<f32>,
    dropped_frames: Arc<AtomicU64>,
}

impl MasterRecorderSink {
    /// Push `samples` (interleaved stereo `f32`, length must be even)
    /// into the ring. Wait-free + alloc-free.
    ///
    /// If the ring can't fit the entire slice, the un-pushed tail's
    /// **frame count** (len/2 of the dropped suffix) is added to the
    /// dropped-frames counter. We still write the prefix that fits so
    /// the file is contiguous up to the overflow point. Recording
    /// continues.
    ///
    /// # ADR-004
    ///
    /// `HeapProd::push_slice` is a memcpy + a release store on the
    /// producer head index. `fetch_add` is a single atomic op. No
    /// allocation, no lock, no syscall.
    #[inline]
    pub fn push(&mut self, samples: &[f32]) {
        let n = self.producer.push_slice(samples);
        if n < samples.len() {
            // Dropped *samples* → /2 to get *frames* (stereo). The
            // counter is in frames so it matches the cpal callback
            // accounting.
            let dropped_samples = (samples.len() - n) as u64;
            let dropped_frames = dropped_samples / 2;
            self.dropped_frames
                .fetch_add(dropped_frames, Ordering::Relaxed);
        }
    }

    /// Total stereo frames dropped due to ring saturation since boot.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }
}

/// Writer-thread side of the recorder. Owned by the control thread
/// (`main.rs`). Holds the join handle for the drainer + the resources
/// needed to finalize the WAV header on [`stop`](Self::stop).
pub struct MasterRecorder {
    /// `None` once `stop()` has been called; makes the impl idempotent.
    inner: Option<MasterRecorderInner>,
    /// Path the recorder writes to. Useful for diagnostics and tests.
    path: PathBuf,
}

struct MasterRecorderInner {
    /// Set to `true` by `stop()` to wake the writer thread out of its
    /// sleep loop early. The writer drains one more time before
    /// returning.
    stop_flag: Arc<AtomicBool>,
    /// Drain thread join handle.
    writer: JoinHandle<Result<u64>>,
    /// Shared drop-counter (read by [`MasterRecorder::dropped_frames`]).
    dropped_frames: Arc<AtomicU64>,
}

impl MasterRecorder {
    /// Open `path` for writing, write a placeholder WAV header, and
    /// spawn the writer thread. The companion [`MasterRecorderSink`]
    /// must be handed to the audio-thread owner.
    ///
    /// # Behaviour
    ///
    /// * Creates parent directories as needed.
    /// * If a file already exists at `path` it is **truncated** — one
    ///   session id maps to exactly one master.wav.
    /// * The writer thread spins at [`WRITER_DRAIN_INTERVAL`] cadence
    ///   draining the ring; it exits when `stop_flag` is set AND the
    ///   ring is empty.
    pub fn new(path: &Path, sample_rate: u32) -> Result<(Self, MasterRecorderSink)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {}", path.display()))?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("opening recording file {}", path.display()))?;

        write_wav_header(&mut file, sample_rate, 0)
            .context("writing initial WAV header (placeholder sizes)")?;

        let rb: HeapRb<f32> = HeapRb::new(RING_CAPACITY_SAMPLES);
        let (producer, consumer) = rb.split();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let dropped_frames = Arc::new(AtomicU64::new(0));

        let writer = spawn_writer(file, consumer, Arc::clone(&stop_flag))?;

        let sink = MasterRecorderSink {
            producer,
            dropped_frames: Arc::clone(&dropped_frames),
        };
        let me = Self {
            inner: Some(MasterRecorderInner {
                stop_flag,
                writer,
                dropped_frames,
            }),
            path: path.to_path_buf(),
        };
        Ok((me, sink))
    }

    /// Open a recorder driven by the [`ENV_RECORDING_DISABLED`] env var.
    /// When disabled, returns `Ok(None)`; the audio thread runs without
    /// a sink and no file is created.
    pub fn try_new_from_env(
        path: &Path,
        sample_rate: u32,
    ) -> Result<Option<(Self, MasterRecorderSink)>> {
        if std::env::var(ENV_RECORDING_DISABLED).as_deref() == Ok("1") {
            return Ok(None);
        }
        Self::new(path, sample_rate).map(Some)
    }

    /// Stop the recorder: signal the writer thread to finish draining,
    /// join it, patch the WAV header with the final data size, and
    /// fsync the file. Idempotent — a second call is a no-op.
    pub fn stop(&mut self) -> Result<()> {
        let Some(inner) = self.inner.take() else {
            return Ok(()); // already stopped
        };

        inner.stop_flag.store(true, Ordering::Release);
        let data_bytes = inner
            .writer
            .join()
            .map_err(|_| anyhow::anyhow!("recording writer thread panicked"))??;

        // Patch RIFF size + data chunk size, then fsync.
        let mut file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .with_context(|| format!("reopening {} to patch header", self.path.display()))?;

        // RIFF size = file_size - 8 (the "RIFF" magic + the size itself).
        let file_size = WAV_HEADER_BYTES + data_bytes;
        let riff_size_le = u32::try_from(file_size - 8)
            .context("file too large for 32-bit WAV (>4 GiB master mix)")?
            .to_le_bytes();
        let data_size_le = u32::try_from(data_bytes)
            .context("data chunk too large for 32-bit WAV (>4 GiB master mix)")?
            .to_le_bytes();

        file.seek(SeekFrom::Start(OFF_RIFF_SIZE))?;
        file.write_all(&riff_size_le)?;
        file.seek(SeekFrom::Start(OFF_DATA_SIZE))?;
        file.write_all(&data_size_le)?;
        file.sync_all().context("fsync recording file")?;

        Ok(())
    }

    /// Path the recorder is writing to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Frames dropped due to ring-overflow back-pressure. Reads the
    /// same atomic the sink increments.
    pub fn dropped_frames(&self) -> u64 {
        match &self.inner {
            Some(i) => i.dropped_frames.load(Ordering::Relaxed),
            None => 0,
        }
    }
}

impl Drop for MasterRecorder {
    fn drop(&mut self) {
        // Best-effort finalization. We can't propagate errors out of
        // Drop; the explicit `stop()` path is preferred.
        if self.inner.is_some() {
            if let Err(e) = self.stop() {
                tracing::warn!(error = %e, "recording: stop on drop failed");
            }
        }
    }
}

/// Spawn the writer thread that drains the ring → file.
fn spawn_writer(
    file: File,
    mut consumer: HeapCons<f32>,
    stop_flag: Arc<AtomicBool>,
) -> Result<JoinHandle<Result<u64>>> {
    let handle = thread::Builder::new()
        .name("hh-recording-writer".to_string())
        .spawn(move || -> Result<u64> {
            // BufWriter amortises the small-write syscalls that
            // `push_slice`-sized drains naturally produce.
            let mut writer = BufWriter::with_capacity(64 * 1024, file);
            // Drain scratch — we copy from the SPSC ring into this
            // buffer and then write_all from it. Fixed size keeps the
            // writer alloc-free per drain. 16 384 f32 = 64 KiB, matches
            // the BufWriter capacity for one syscall per pass.
            let mut scratch = vec![0.0f32; 16_384];
            let mut total_bytes = 0u64;
            loop {
                let n = consumer.pop_slice(&mut scratch);
                if n > 0 {
                    // `bytemuck::cast_slice` reinterprets `&[f32]` as
                    // `&[u8]` with zero cost (no copy, no alloc — same
                    // contract the audio thread relies on). The cast is
                    // trait-checked at compile time via `Pod` +
                    // `AnyBitPattern` on `f32`; no `unsafe` lives at
                    // the call site, satisfying CLAUDE.md's "no unsafe
                    // without an ADR" rule. Issue #45.
                    let bytes: &[u8] = bytemuck::cast_slice(&scratch[..n]);
                    writer.write_all(bytes).context("write PCM body")?;
                    total_bytes += bytes.len() as u64;
                }
                let stopping = stop_flag.load(Ordering::Acquire);
                if n == 0 {
                    if stopping {
                        // Final occupancy check is built into the loop:
                        // we already saw an empty pop AND the stop flag
                        // is high → safe to exit.
                        break;
                    }
                    thread::sleep(WRITER_DRAIN_INTERVAL);
                } else if stopping && consumer.occupied_len() == 0 {
                    break;
                }
            }
            writer.flush().context("flush BufWriter")?;
            Ok(total_bytes)
        })
        .context("spawning recording writer thread")?;
    Ok(handle)
}

/// Write the WAV header for a PCM-IEEE-float stereo file. The size
/// fields are written as `data_size` (and `file_size - 8` derived from
/// it); pass 0 to write placeholder zeros, then patch after the body
/// is written.
///
/// Layout (44 bytes total, all little-endian):
///
/// | off | bytes | field            | value                       |
/// |----:|------:|------------------|-----------------------------|
/// |   0 |     4 | RIFF magic       | `"RIFF"`                    |
/// |   4 |     4 | RIFF size        | `file_size - 8`             |
/// |   8 |     4 | WAVE magic       | `"WAVE"`                    |
/// |  12 |     4 | fmt chunk magic  | `"fmt "`                    |
/// |  16 |     4 | fmt chunk size   | `16`                        |
/// |  20 |     2 | format           | `3` (IEEE float)            |
/// |  22 |     2 | channels         | `2`                         |
/// |  24 |     4 | sample_rate      | from cpal                   |
/// |  28 |     4 | byte_rate        | `sample_rate * 2 * 4`       |
/// |  32 |     2 | block_align      | `8` (2 ch × 4 bytes)        |
/// |  34 |     2 | bits_per_sample  | `32`                        |
/// |  36 |     4 | data chunk magic | `"data"`                    |
/// |  40 |     4 | data size        | PCM body bytes              |
pub fn write_wav_header(file: &mut File, sample_rate: u32, data_size: u32) -> Result<()> {
    const CHANNELS: u16 = 2;
    const BITS: u16 = 32;
    const FMT_IEEE_FLOAT: u16 = 3;
    let byte_rate = sample_rate * u32::from(CHANNELS) * u32::from(BITS / 8);
    let block_align: u16 = CHANNELS * (BITS / 8);

    let mut hdr = [0u8; WAV_HEADER_BYTES as usize];
    hdr[0..4].copy_from_slice(b"RIFF");
    let riff_size = WAV_HEADER_BYTES.saturating_sub(8) as u32 + data_size;
    hdr[4..8].copy_from_slice(&riff_size.to_le_bytes());
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes());
    hdr[20..22].copy_from_slice(&FMT_IEEE_FLOAT.to_le_bytes());
    hdr[22..24].copy_from_slice(&CHANNELS.to_le_bytes());
    hdr[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    hdr[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    hdr[32..34].copy_from_slice(&block_align.to_le_bytes());
    hdr[34..36].copy_from_slice(&BITS.to_le_bytes());
    hdr[36..40].copy_from_slice(b"data");
    hdr[40..44].copy_from_slice(&data_size.to_le_bytes());

    file.write_all(&hdr).context("write WAV header bytes")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::AtomicUsize;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Per-test scratch dir, isolated by pid + nanos.
    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hh-rec-{tag}-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Read a 44-byte WAV header from `path` and parse the fields we
    /// care about for the round-trip + spec tests. Returns
    /// `(sample_rate, channels, bits, format, data_size, body_bytes)`.
    fn read_wav(path: &Path) -> (u32, u16, u16, u16, u32, Vec<u8>) {
        let mut f = File::open(path).expect("open wav");
        let mut hdr = [0u8; 44];
        f.read_exact(&mut hdr).expect("read header");
        assert_eq!(&hdr[0..4], b"RIFF");
        assert_eq!(&hdr[8..12], b"WAVE");
        assert_eq!(&hdr[12..16], b"fmt ");
        assert_eq!(&hdr[36..40], b"data");
        let format = u16::from_le_bytes([hdr[20], hdr[21]]);
        let channels = u16::from_le_bytes([hdr[22], hdr[23]]);
        let sample_rate = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]);
        let bits = u16::from_le_bytes([hdr[34], hdr[35]]);
        let data_size = u32::from_le_bytes([hdr[40], hdr[41], hdr[42], hdr[43]]);
        let mut body = Vec::new();
        f.read_to_end(&mut body).expect("read body");
        (sample_rate, channels, bits, format, data_size, body)
    }

    fn decode_pcm_f32(body: &[u8]) -> Vec<f32> {
        assert_eq!(body.len() % 4, 0, "PCM body not a multiple of f32 size");
        body.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    /// 1: Header bytes match spec exactly when stop() is called after
    /// pushing a known small number of samples.
    #[test]
    fn wav_header_matches_spec_after_stop() {
        let dir = scratch_dir("hdr");
        let path = dir.join("master.wav");
        let (mut rec, mut sink) = MasterRecorder::new(&path, 48_000).expect("new");
        // 1000 stereo samples = 500 frames; body = 4000 bytes.
        let samples = vec![0.0f32; 1000];
        sink.push(&samples);
        rec.stop().expect("stop");
        let (sr, ch, bits, fmt, data_size, body) = read_wav(&path);
        assert_eq!(sr, 48_000);
        assert_eq!(ch, 2);
        assert_eq!(bits, 32);
        assert_eq!(fmt, 3, "PCM IEEE float");
        assert_eq!(data_size, 4000);
        assert_eq!(body.len(), 4000);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 2: Round-trip — push exactly 1 second of stereo @ 48 kHz, stop,
    /// re-read body, assert sample-for-sample equality with what was
    /// pushed.
    #[test]
    fn round_trip_one_second_48k_stereo() {
        let dir = scratch_dir("rt");
        let path = dir.join("master.wav");
        let (mut rec, mut sink) = MasterRecorder::new(&path, 48_000).expect("new");
        // 48 000 stereo frames = 96 000 interleaved f32. Use a
        // varying-but-deterministic test signal so silent-write bugs
        // can't pass.
        let mut input = Vec::with_capacity(96_000);
        for i in 0..48_000 {
            let l = (i as f32) * 1e-4;
            let r = -(i as f32) * 1e-4;
            input.push(l);
            input.push(r);
        }
        // Push in 1024-frame chunks to stress the writer-drain loop +
        // mimic real cpal callbacks. Sleep 1 ms between chunks so the
        // writer thread keeps up.
        for chunk in input.chunks(2048) {
            sink.push(chunk);
            thread::sleep(Duration::from_millis(1));
        }
        rec.stop().expect("stop");
        let (_sr, _ch, _bits, _fmt, data_size, body) = read_wav(&path);
        assert_eq!(data_size as usize, body.len());
        let got = decode_pcm_f32(&body);
        assert_eq!(got.len(), input.len(), "round-trip sample count");
        // Sample-for-sample equality (no quantization — IEEE-float).
        for (i, (a, b)) in input.iter().zip(got.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1e-9,
                "sample {i} mismatch: pushed {a} got {b}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 3: Ring overflow — push faster than the writer can drain on a
    /// recorder with a tiny ring; assert the dropped-frame counter
    /// increments and the file is still well-formed.
    #[test]
    fn ring_overflow_increments_dropped_counter_and_continues() {
        let dir = scratch_dir("ov");
        let path = dir.join("master.wav");
        // Use the public API but throttle the writer by giving it the
        // standard ring; we then push more than the ring can hold in
        // a single tight loop without sleeping so the writer has no
        // chance to drain.
        let (mut rec, mut sink) = MasterRecorder::new(&path, 48_000).expect("new");
        // 4× the ring capacity in samples — guaranteed overflow.
        let glut = vec![0.5f32; RING_CAPACITY_SAMPLES * 4];
        sink.push(&glut);
        // The push may have partially fit; that's fine. The counter is
        // what we assert on.
        assert!(
            sink.dropped_frames() > 0,
            "expected drops when pushing 4× ring capacity in one shot"
        );
        // Recording must still be live — push a few more samples and
        // stop cleanly.
        sink.push(&[0.0f32, 0.0f32]);
        rec.stop().expect("stop after overflow");
        let (_sr, _ch, _bits, _fmt, _data_size, body) = read_wav(&path);
        assert!(!body.is_empty(), "some samples should have been written");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 4: Disabled mode — env var set → `try_new_from_env` returns
    /// `Ok(None)` and no file is created.
    #[test]
    fn disabled_mode_returns_none_and_creates_no_file() {
        let dir = scratch_dir("dis");
        let path = dir.join("master.wav");
        // SAFETY: this test runs single-threaded w.r.t. env vars; the
        // recording tests don't otherwise touch this env var.
        std::env::set_var(ENV_RECORDING_DISABLED, "1");
        let out = MasterRecorder::try_new_from_env(&path, 48_000).expect("try_new");
        std::env::remove_var(ENV_RECORDING_DISABLED);
        assert!(out.is_none(), "expected None in disabled mode");
        assert!(!path.exists(), "no file should be created in disabled mode");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 5: Stop is idempotent — calling stop twice doesn't double-patch
    /// the header (which would corrupt sizes) or panic.
    #[test]
    fn stop_is_idempotent() {
        let dir = scratch_dir("idem");
        let path = dir.join("master.wav");
        let (mut rec, mut sink) = MasterRecorder::new(&path, 48_000).expect("new");
        sink.push(&[0.1f32, 0.2f32, 0.3f32, 0.4f32]);
        rec.stop().expect("first stop");
        let (_sr, _ch, _bits, _fmt, data_size_a, body_a) = read_wav(&path);
        rec.stop().expect("second stop (no-op)");
        let (_sr, _ch, _bits, _fmt, data_size_b, body_b) = read_wav(&path);
        assert_eq!(data_size_a, data_size_b, "data_size must not change");
        assert_eq!(body_a, body_b, "body must not change");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 6: Audio-thread `push` is alloc-free. The contract per ADR-004
    /// requires the producer side to never allocate after construction.
    #[test]
    fn push_is_alloc_free() {
        let dir = scratch_dir("nalloc");
        let path = dir.join("master.wav");
        let (mut rec, mut sink) = MasterRecorder::new(&path, 48_000).expect("new");
        // Pre-allocate the input outside the gated scope.
        let buf = [0.25f32; 2048];
        assert_no_alloc::assert_no_alloc(|| {
            // The hot path: a few hundred pushes back-to-back, exactly
            // mimicking the audio-thread cadence (~10 calls/s @ 1024
            // frames = sub-microsecond per call).
            for _ in 0..200 {
                sink.push(&buf);
            }
        });
        rec.stop().expect("stop");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 7: Measured push latency on a 1024-frame stereo block stays
    /// well under the 5 µs budget called out in the PR brief. This is
    /// not a hard gate in debug builds (allocator + page faults can
    /// jitter), but the budget is generous: a memcpy of 8 KiB on
    /// modern hardware is ≪ 1 µs.
    #[test]
    fn push_latency_under_budget_1024_block() {
        let dir = scratch_dir("lat");
        let path = dir.join("master.wav");
        let (mut rec, mut sink) = MasterRecorder::new(&path, 48_000).expect("new");
        let buf = [0.0f32; 2048];
        // Warm up the page tables.
        sink.push(&buf);
        let mut worst = Duration::ZERO;
        for _ in 0..1000 {
            let t = std::time::Instant::now();
            sink.push(&buf);
            let dt = t.elapsed();
            if dt > worst {
                worst = dt;
            }
        }
        // CI runners (macOS in particular) jitter well past 50µs under
        // contention — observed 409µs on PR #87 (rev 2 of this bump).
        // Widen tolerance for debug builds to 2ms so the test doesn't
        // flake under any reasonable load. Production budget stays
        // tight in release (5µs).
        let budget = if cfg!(debug_assertions) {
            Duration::from_micros(2_000)
        } else {
            Duration::from_micros(5)
        };
        eprintln!("[rec-latency] push 1024 frames worst = {worst:?}, budget = {budget:?}");
        assert!(
            worst <= budget,
            "push exceeded latency budget: worst {worst:?} > {budget:?}"
        );
        rec.stop().expect("stop");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 8b (issue #45): `bytemuck::cast_slice::<f32, u8>` produces the
    /// exact same byte sequence we would get from explicit per-sample
    /// `f32::to_le_bytes` encoding. This pins down the contract the
    /// previous handwritten `unsafe` slice-reinterpret was satisfying,
    /// so the swap is a behaviour-preserving cleanup (not a format
    /// change). Targets are all little-endian (x86_64 / aarch64
    /// macOS / aarch64 Linux); if a future target adds a big-endian
    /// host, this test will fail loudly and we revisit the cast.
    #[test]
    fn bytemuck_cast_matches_raw_bytes() {
        // Mixed-sign, sub-normal, NaN-free signal — exercises the full
        // mantissa range without relying on bit-pattern equality of
        // NaNs (which is impl-defined).
        let samples: Vec<f32> = vec![
            0.0,
            1.0,
            -1.0,
            0.5,
            -0.5,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            std::f32::consts::PI,
            -std::f32::consts::E,
            1e-9,
            1e9,
            f32::EPSILON,
        ];

        // Reference: per-sample little-endian encoding.
        let mut expected = Vec::with_capacity(samples.len() * 4);
        for s in &samples {
            expected.extend_from_slice(&s.to_le_bytes());
        }

        // Under test: bytemuck zero-copy view.
        let got: &[u8] = bytemuck::cast_slice(&samples);

        assert_eq!(
            got.len(),
            samples.len() * std::mem::size_of::<f32>(),
            "cast_slice length must scale by sizeof(f32)"
        );
        assert_eq!(
            got, expected,
            "bytemuck::cast_slice bytes must match f32::to_le_bytes encoding on this target"
        );
    }

    /// 8: Push after stop — the sink can still accept pushes without
    /// panic; the file is already finalized so the extra samples are
    /// silently dropped. This matches the cpal-callback shutdown
    /// ordering: the stream may emit one more buffer between
    /// `recorder.stop()` and the stream actually being dropped.
    #[test]
    fn push_after_stop_does_not_panic() {
        let dir = scratch_dir("aftstop");
        let path = dir.join("master.wav");
        let (mut rec, mut sink) = MasterRecorder::new(&path, 48_000).expect("new");
        sink.push(&[0.0f32, 0.0f32]);
        rec.stop().expect("stop");
        // Drop counter may bump; we just assert no panic + no UB.
        sink.push(&[0.0f32; 1024]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
