//! Crowd-pleaser export — auto-trim the master mix + emit chapter markers
//! at every `DeckLoad`.
//!
//! Post-session, a DJ wants to share their set. Two ergonomic asks:
//!
//! 1. The raw `master.wav` has 5-30 s of dead air at the head + tail
//!    (booth open, cable wiggling, "is it recording?" — silence). Trim it.
//! 2. The set is N tracks; a streaming platform / podcast app wants
//!    chapter markers so listeners can jump. The event log already
//!    records every `DeckLoad`; convert those event timestamps into
//!    PCM-frame offsets in the trimmed WAV and emit an FFmpeg-format
//!    chapters sidecar.
//!
//! # Silence detector
//!
//! RMS-over-window, threshold at **-60 dBFS** sustained for **> 2 s**.
//! Rationale:
//!
//! * **-60 dBFS**: a quiet booth's noise floor sits around -70 to -80 dBFS;
//!   a 16-bit dithered file's noise floor sits at -96 dBFS. -60 dBFS is
//!   well above either — anything quieter is real silence (mic off, no
//!   audio routing) rather than ambient noise that the DJ might still
//!   want preserved (drums about to drop in from a -40 dB tease, for
//!   instance). Empirically also matches `ffmpeg silenceremove` defaults
//!   (`-30 dB` for "noisy" / `-60 dB` for "studio").
//! * **2 s window**: shorter windows trim intentional dramatic pauses
//!   (the `Mr. Brightside` 4-second cliffhanger before the kick). 2 s
//!   is comfortably longer than the longest drop-out a human ear
//!   tolerates as "still playing" and short enough that a 30 s booth-
//!   open warm-up still gets clipped.
//!
//! Detection runs **frame-windowed** (not sample-windowed): we accumulate
//! squared magnitudes over a 2-second window, root the mean, then convert
//! to dBFS via `20·log10(rms)`. Stereo is collapsed to mono via the L+R
//! mean before squaring — a hard-panned silent channel doesn't fool the
//! detector when the other channel carries audio.
//!
//! # Chapter format
//!
//! Output is **FFmpeg metadata format** — the same format `ffmpeg
//! -f ffmetadata` consumes, so the user can mux chapters into an AAC /
//! M4A export downstream with one command. Spec:
//! <https://ffmpeg.org/ffmpeg-formats.html#Metadata-1>.
//!
//! ```text
//! ;FFMETADATA1
//! [CHAPTER]
//! TIMEBASE=1/1000
//! START=0
//! END=180000
//! title=Track Title
//! ```
//!
//! Each `DeckLoad` event in the events.jsonl becomes one chapter; the
//! chapter's START is the event's `ts_micros` converted to ms-offset
//! relative to the **start of the trimmed output** (so chapter 0 is at
//! ms 0 in the exported WAV, not at the wall-clock ts of the first
//! event). END is the next chapter's START, or the WAV's full duration
//! for the final chapter.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::state::{Event, EventKind};

use super::write_wav_header;

/// RMS threshold below which a window counts as "silent". -60 dBFS as a
/// linear amplitude is `10^(-60/20) = 1e-3`. Comparing against the
/// squared form skips a sqrt per window.
const SILENCE_DBFS: f32 = -60.0;

/// Sustained-silence window length, in seconds. A window shorter than
/// this is treated as a normal in-music gap (drop, breakdown).
const SILENCE_WINDOW_S: f32 = 2.0;

/// Per-deck-load chapter row in the FFmpeg metadata file.
#[derive(Debug, Clone, PartialEq)]
struct Chapter {
    /// Start offset in the **trimmed** wav, in milliseconds.
    start_ms: u64,
    /// Display title — the track id from the DeckLoad event.
    title: String,
}

/// Summary of an export run. Returned by [`export_session`] and
/// surfaced over the bridge so the UI can show a toast.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ExportSummary {
    /// Duration of the source `master.wav` before trimming.
    pub input_duration_s: f64,
    /// Duration of the written output WAV after trimming.
    pub output_duration_s: f64,
    /// Seconds clipped off the head.
    pub trimmed_head_s: f64,
    /// Seconds clipped off the tail.
    pub trimmed_tail_s: f64,
    /// Count of DeckLoad chapters in the emitted sidecar.
    pub chapter_count: u32,
    /// Absolute path of the written output WAV (so the bridge can echo
    /// it back to the UI as a "Saved to …" download hint).
    pub output_path: String,
    /// Absolute path of the chapters sidecar.
    pub chapters_path: String,
}

/// Public entry point: read the session's `master.wav` + `events.jsonl`,
/// trim leading/trailing silence, write a new WAV to `output_path`, and
/// emit `<output_path>.chapters.txt`.
///
/// `output_path` must be writable. Parent directories are created.
pub fn export_session(session_id: &str, output_path: &Path) -> Result<ExportSummary> {
    let root = crate::persistence::sessions::resolve_root()
        .context("could not resolve persistence root for export")?;
    export_session_in(&root, session_id, output_path)
}

/// Testable variant of [`export_session`] that takes an explicit
/// persistence root instead of consulting env vars.
pub fn export_session_in(
    root: &Path,
    session_id: &str,
    output_path: &Path,
) -> Result<ExportSummary> {
    crate::persistence::sessions::validate_session_id(session_id).context("invalid session id")?;
    let dir = root.join(session_id);
    if !dir.is_dir() {
        anyhow::bail!("session not found: {session_id}");
    }
    let wav_path = dir.join(crate::persistence::sessions::MASTER_WAV_FILENAME);
    if !wav_path.is_file() {
        anyhow::bail!("master.wav missing for session {session_id}");
    }

    // 1. Load + decode the WAV into memory. Master mixes for a club
    //    set top out at a few hundred MB — bounded enough to slurp.
    let (sample_rate, body) =
        read_wav_pcm_f32(&wav_path).with_context(|| format!("reading {}", wav_path.display()))?;
    if body.is_empty() {
        anyhow::bail!("master.wav is empty for session {session_id}");
    }

    // 2. Detect silence head + tail.
    let (head_frames, tail_frames) = detect_silence_trim(&body, sample_rate);
    let total_frames = body.len() / 2;
    if head_frames + tail_frames >= total_frames {
        anyhow::bail!(
            "master.wav is silent end-to-end (head={head_frames}, tail={tail_frames}, total={total_frames})"
        );
    }
    let trimmed_body = &body[head_frames * 2..(total_frames - tail_frames) * 2];
    let trimmed_frames = trimmed_body.len() / 2;
    let input_duration_s = total_frames as f64 / f64::from(sample_rate);
    let output_duration_s = trimmed_frames as f64 / f64::from(sample_rate);
    let head_s = head_frames as f64 / f64::from(sample_rate);
    let tail_s = tail_frames as f64 / f64::from(sample_rate);

    // 3. Write the trimmed WAV.
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {}", output_path.display()))?;
        }
    }
    write_trimmed_wav(output_path, sample_rate, trimmed_body)
        .with_context(|| format!("writing trimmed wav {}", output_path.display()))?;

    // 4. Build chapters from the event log.
    let events_path = dir.join("events.jsonl");
    let chapters = if events_path.exists() {
        build_chapters(&events_path, head_s, output_duration_s).with_context(|| {
            format!(
                "reading events.jsonl for chapters: {}",
                events_path.display()
            )
        })?
    } else {
        Vec::new()
    };
    let chapters_path = chapters_sidecar_path(output_path);
    write_chapters(&chapters_path, &chapters, output_duration_s)
        .with_context(|| format!("writing chapters sidecar {}", chapters_path.display()))?;

    Ok(ExportSummary {
        input_duration_s,
        output_duration_s,
        trimmed_head_s: head_s,
        trimmed_tail_s: tail_s,
        chapter_count: chapters.len() as u32,
        output_path: output_path.to_string_lossy().into_owned(),
        chapters_path: chapters_path.to_string_lossy().into_owned(),
    })
}

/// Read a 44-byte WAV header + PCM IEEE-float stereo body. Returns the
/// sample rate + the body as a `Vec<f32>` (interleaved stereo). Rejects
/// formats this engine doesn't emit — keeps the surface small.
fn read_wav_pcm_f32(path: &Path) -> Result<(u32, Vec<f32>)> {
    let mut file = BufReader::new(File::open(path)?);
    let mut hdr = [0u8; 44];
    file.read_exact(&mut hdr).context("read header")?;
    if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" || &hdr[12..16] != b"fmt " {
        anyhow::bail!("not a RIFF/WAVE/fmt file");
    }
    let format = u16::from_le_bytes([hdr[20], hdr[21]]);
    let channels = u16::from_le_bytes([hdr[22], hdr[23]]);
    let sample_rate = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]);
    let bits = u16::from_le_bytes([hdr[34], hdr[35]]);
    if format != 3 || channels != 2 || bits != 32 {
        anyhow::bail!(
            "unsupported wav: format={format} channels={channels} bits={bits} (need IEEE-float stereo 32-bit)"
        );
    }
    let data_size = u32::from_le_bytes([hdr[40], hdr[41], hdr[42], hdr[43]]) as usize;
    let mut body_bytes = Vec::with_capacity(data_size);
    file.read_to_end(&mut body_bytes).context("read body")?;
    // Tolerant: if `data_size` is a placeholder (some writers leave 0),
    // fall back to "read whatever the file holds".
    let take = if data_size == 0 || data_size > body_bytes.len() {
        body_bytes.len()
    } else {
        data_size
    };
    let body_bytes = &body_bytes[..take];
    if body_bytes.len() % 4 != 0 {
        anyhow::bail!("wav body not a multiple of f32 size");
    }
    // Per-sample decode keeps the function free of unsafe + bytemuck
    // borrowing edges; the cost is tiny vs. disk I/O.
    let mut samples = Vec::with_capacity(body_bytes.len() / 4);
    for c in body_bytes.chunks_exact(4) {
        samples.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
    Ok((sample_rate, samples))
}

/// Find the **frame** offsets to trim off the head and tail.
///
/// Returns `(head_frames, tail_frames)`. Adding the two and subtracting
/// from total frames must yield the kept-frame count.
///
/// Algorithm: classify the audio into 100 ms "blocks" by RMS, then
/// trim off **contiguous silent blocks** at the head + tail. A block
/// counts as silent when its RMS is below the -60 dBFS threshold;
/// trim continues until a run of silent blocks **at least 2 s long**
/// is broken by an audible block.
///
/// This gives sub-block precision at the silence/audio boundary
/// (the trim lands within one 100 ms block of the true audio onset)
/// AND respects the 2-second-sustained rule from the docstring —
/// short pauses inside the music (a beat drop) never bisect the
/// trim because we only trim CONTIGUOUS silence from the edges.
fn detect_silence_trim(body_interleaved: &[f32], sample_rate: u32) -> (usize, usize) {
    let total_frames = body_interleaved.len() / 2;
    if total_frames == 0 {
        return (0, 0);
    }
    let block_frames = (f64::from(sample_rate) * 0.1) as usize; // 100 ms
    let block_frames = block_frames.max(1);
    if total_frames < block_frames {
        // Single sub-block — nothing meaningful to trim.
        return (0, 0);
    }
    let blocks = total_frames / block_frames;
    let threshold = 10f32.powf(SILENCE_DBFS / 20.0); // linear amplitude
    let threshold_sq = (threshold as f64) * (threshold as f64);

    // Sustained-silence floor: we only "commit" to trimming if at least
    // this many contiguous silent blocks form the head/tail run.
    let min_silent_blocks = ((f64::from(SILENCE_WINDOW_S) * 1000.0) / 100.0).ceil() as usize;

    // Classify every block.
    let is_silent: Vec<bool> = (0..blocks)
        .map(|i| {
            let start = i * block_frames;
            window_rms_sq(body_interleaved, start, block_frames) < threshold_sq
        })
        .collect();

    // Count contiguous silent blocks at head + tail.
    let head_silent_blocks = is_silent.iter().take_while(|s| **s).count();
    let tail_silent_blocks = is_silent.iter().rev().take_while(|s| **s).count();

    // Apply the sustained-rule: only trim when the contiguous run is
    // at least `min_silent_blocks` long. Below that, the silence is
    // probably musical (a breakdown / build) and we leave it alone.
    let head_frames = if head_silent_blocks >= min_silent_blocks {
        head_silent_blocks * block_frames
    } else {
        0
    };
    let tail_frames = if tail_silent_blocks >= min_silent_blocks {
        tail_silent_blocks * block_frames
    } else {
        0
    };
    (head_frames, tail_frames)
}

/// Mean of squared mono-collapsed magnitudes over a frame window.
/// Returns mean-square (we compare against `threshold²` to avoid a sqrt).
fn window_rms_sq(body: &[f32], start_frame: usize, n_frames: usize) -> f64 {
    let mut acc = 0.0f64;
    let end_frame = start_frame + n_frames;
    for f in start_frame..end_frame {
        let l = body[f * 2] as f64;
        let r = body[f * 2 + 1] as f64;
        let mono = 0.5 * (l + r);
        acc += mono * mono;
    }
    acc / n_frames as f64
}

/// Write a fresh WAV at `path` containing `body` (interleaved stereo
/// f32 samples). Uses [`super::write_wav_header`] for byte-perfect
/// compatibility with the recorder's own format.
fn write_trimmed_wav(path: &Path, sample_rate: u32, body: &[f32]) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("create output {}", path.display()))?;
    let data_bytes =
        u32::try_from(body.len() * 4).context("output exceeds 4 GiB WAV size limit")?;
    write_wav_header(&mut file, sample_rate, data_bytes)?;
    let mut writer = BufWriter::with_capacity(64 * 1024, file);
    // bytemuck is whitelisted in CLAUDE.md for f32→u8 reinterpret —
    // same path the recorder writer uses.
    let bytes: &[u8] = bytemuck::cast_slice(body);
    writer.write_all(bytes).context("write trimmed body")?;
    writer.flush().context("flush trimmed body")?;
    Ok(())
}

/// `<output_path>.chapters.txt` — sibling sidecar.
fn chapters_sidecar_path(output_path: &Path) -> PathBuf {
    let mut s = output_path.as_os_str().to_owned();
    s.push(".chapters.txt");
    PathBuf::from(s)
}

/// Read `events.jsonl`, keep only `DeckLoad` events, convert each event's
/// ms-offset-from-session-start to ms-offset-in-trimmed-output, and
/// build the chapter list.
///
/// `head_trim_s` = seconds clipped off the head (so a DeckLoad at 7 s
/// in the source maps to 2 s in the output when we trimmed 5 s).
/// `output_duration_s` = total trimmed duration (chapters past the
/// trimmed tail are dropped; chapters before the trimmed head are
/// clamped to 0 so a pre-roll deck load still surfaces as the first
/// chapter).
fn build_chapters(
    events_path: &Path,
    head_trim_s: f64,
    output_duration_s: f64,
) -> Result<Vec<Chapter>> {
    let file = File::open(events_path)?;
    let reader = BufReader::new(file);
    use std::io::BufRead;
    let mut first_ts_micros: Option<i64> = None;
    let mut chapters: Vec<Chapter> = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ev: Event = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if first_ts_micros.is_none() {
            first_ts_micros = Some(ev.ts_micros);
        }
        if let EventKind::DeckLoad { track, .. } = &ev.kind {
            let offset_micros = ev
                .ts_micros
                .saturating_sub(first_ts_micros.unwrap_or(ev.ts_micros));
            let offset_s = (offset_micros as f64) / 1_000_000.0;
            let in_output_s = offset_s - head_trim_s;
            // Clamp: pre-head loads → ms 0; past-tail loads → drop.
            if in_output_s > output_duration_s {
                continue;
            }
            let start_ms = if in_output_s < 0.0 {
                0u64
            } else {
                (in_output_s * 1000.0).round() as u64
            };
            chapters.push(Chapter {
                start_ms,
                title: track.id.clone(),
            });
        }
    }
    // De-dup chapters with identical start_ms (a noisy log can hold two
    // DeckLoads in the same ms; ffmpeg rejects zero-duration chapters).
    chapters.sort_by_key(|c| c.start_ms);
    chapters.dedup_by(|a, b| a.start_ms == b.start_ms);
    Ok(chapters)
}

/// Write the FFmpeg-metadata chapters sidecar. Format spec:
/// <https://ffmpeg.org/ffmpeg-formats.html#Metadata-1>.
fn write_chapters(path: &Path, chapters: &[Chapter], output_duration_s: f64) -> Result<()> {
    let mut f = BufWriter::new(File::create(path)?);
    writeln!(f, ";FFMETADATA1")?;
    let total_ms = (output_duration_s * 1000.0).round() as u64;
    for (i, c) in chapters.iter().enumerate() {
        let end = chapters
            .get(i + 1)
            .map(|n| n.start_ms)
            .unwrap_or(total_ms.max(c.start_ms + 1));
        writeln!(f, "[CHAPTER]")?;
        writeln!(f, "TIMEBASE=1/1000")?;
        writeln!(f, "START={}", c.start_ms)?;
        writeln!(f, "END={end}")?;
        // ffmpeg's metadata format expects `=` and newlines escaped; the
        // track ids we emit are short alphanumeric so we sanitize
        // defensively rather than fully escape.
        let safe_title: String = c
            .title
            .chars()
            .map(|ch| match ch {
                '\n' | '\r' | '=' | ';' | '#' | '\\' => ' ',
                other => other,
            })
            .collect();
        writeln!(f, "title={safe_title}")?;
    }
    f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeckId, EventSource, TrackRef};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const SR: u32 = 48_000;

    fn scratch_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        static C: AtomicUsize = AtomicUsize::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hh-export-{tag}-{pid}-{nanos}-{n}"));
        fs::create_dir_all(&dir).expect("scratch root");
        dir
    }

    /// Write a stereo IEEE-float WAV at `path` with one tone segment
    /// embedded between leading + trailing silence.
    fn write_test_wav(
        path: &Path,
        leading_silence_s: f32,
        tone_s: f32,
        trailing_silence_s: f32,
    ) -> usize {
        let leading_frames = (leading_silence_s * SR as f32) as usize;
        let tone_frames = (tone_s * SR as f32) as usize;
        let trailing_frames = (trailing_silence_s * SR as f32) as usize;
        let total_frames = leading_frames + tone_frames + trailing_frames;
        let data_bytes = (total_frames * 2 * 4) as u32;
        let mut file = File::create(path).expect("create wav");
        write_wav_header(&mut file, SR, data_bytes).expect("header");
        // Silence head.
        for _ in 0..leading_frames {
            file.write_all(&0.0f32.to_le_bytes()).unwrap();
            file.write_all(&0.0f32.to_le_bytes()).unwrap();
        }
        // Tone — sine wave at 440 Hz, amplitude 0.5 (well above -60 dBFS).
        for i in 0..tone_frames {
            let t = (i as f32) / SR as f32;
            let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            file.write_all(&s.to_le_bytes()).unwrap();
            file.write_all(&s.to_le_bytes()).unwrap();
        }
        // Silence tail.
        for _ in 0..trailing_frames {
            file.write_all(&0.0f32.to_le_bytes()).unwrap();
            file.write_all(&0.0f32.to_le_bytes()).unwrap();
        }
        total_frames
    }

    fn seed_session(
        root: &Path,
        session_id: &str,
        leading_silence_s: f32,
        tone_s: f32,
        trailing_silence_s: f32,
        deckload_ts_offsets_s: &[(f32, &str)],
    ) {
        let dir = root.join(session_id);
        fs::create_dir_all(&dir).unwrap();
        write_test_wav(
            &dir.join(crate::persistence::sessions::MASTER_WAV_FILENAME),
            leading_silence_s,
            tone_s,
            trailing_silence_s,
        );
        // Anchor t=0 at 1_000_000 micros so subtraction stays positive.
        let t0 = 1_000_000i64;
        let mut events: Vec<Event> = vec![Event {
            id: 1,
            ts_micros: t0,
            source: EventSource::Ui,
            kind: EventKind::SessionStart,
        }];
        for (i, (offset_s, track_id)) in deckload_ts_offsets_s.iter().enumerate() {
            let ts = t0 + ((offset_s * 1_000_000.0) as i64);
            events.push(Event {
                id: 2 + i as u64,
                ts_micros: ts,
                source: EventSource::Ui,
                kind: EventKind::DeckLoad {
                    deck: DeckId::A,
                    track: TrackRef {
                        id: (*track_id).into(),
                        path: format!("/m/{track_id}.mp3"),
                    },
                    bpm: 128.0,
                    beat_grid_anchor_ms: 0,
                    downbeats_ms: vec![],
                    hot_cues: [None; 8],
                    track_gain_db: 0.0,
                },
            });
        }
        let mut f = File::create(dir.join("events.jsonl")).unwrap();
        for e in &events {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
        }
    }

    #[test]
    fn export_trims_leading_and_trailing_silence_to_audible_body() {
        let root = scratch_root("trim-basic");
        let sid = "20260518T000000Z-aaaa";
        // 5s silence + 22s tone + 3s silence = 30s total → trim to 22s.
        seed_session(&root, sid, 5.0, 22.0, 3.0, &[]);
        let out = root.join("export.wav");
        let summary = export_session_in(&root, sid, &out).expect("export ok");
        // Within one window-step (≈ 0.5s).
        assert!(
            (summary.input_duration_s - 30.0).abs() < 0.05,
            "input duration {} ≠ 30.0",
            summary.input_duration_s
        );
        // 100 ms-block precision: trim lands within one block of the
        // true silence boundary (so output is 22.0 ± 0.2 s).
        assert!(
            (summary.output_duration_s - 22.0).abs() < 0.25,
            "output duration {} ≠ ~22.0",
            summary.output_duration_s
        );
        assert!(summary.trimmed_head_s >= 4.8 && summary.trimmed_head_s <= 5.1);
        assert!(summary.trimmed_tail_s >= 2.8 && summary.trimmed_tail_s <= 3.1);
        // Output WAV exists + parses with matching duration.
        let (sr, body) = read_wav_pcm_f32(&out).expect("read back");
        assert_eq!(sr, SR);
        let dur = (body.len() / 2) as f64 / f64::from(sr);
        assert!((dur - summary.output_duration_s).abs() < 1e-6);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn export_writes_chapters_at_deck_load_offsets_relative_to_trim() {
        let root = scratch_root("chapters");
        let sid = "20260518T000001Z-bbbb";
        // 5s head silence, 22s tone, 3s tail. DeckLoad at 10s into the
        // session ⇒ 10s - 5s = 5s into the trimmed output ⇒ start_ms=5000.
        seed_session(&root, sid, 5.0, 22.0, 3.0, &[(10.0, "anthem")]);
        let out = root.join("export.wav");
        let summary = export_session_in(&root, sid, &out).expect("export ok");
        assert_eq!(summary.chapter_count, 1);
        let sidecar = chapters_sidecar_path(&out);
        let text = fs::read_to_string(&sidecar).expect("sidecar");
        assert!(text.starts_with(";FFMETADATA1"), "header missing:\n{text}");
        assert!(text.contains("[CHAPTER]"));
        assert!(text.contains("TIMEBASE=1/1000"));
        assert!(text.contains("title=anthem"));
        // Trim head is ~5s → in-output offset ~5000 ms (slack for the
        // silence-window step).
        let start_line = text
            .lines()
            .find(|l| l.starts_with("START="))
            .expect("START= line");
        let start_ms: i64 = start_line.trim_start_matches("START=").parse().unwrap();
        // DeckLoad at 10s → with ~5s head trim, lands at ~5000 ms.
        // 100 ms-block precision: allow ±200 ms tolerance.
        assert!(
            (4800..=5200).contains(&start_ms),
            "expected ~5000 ms, got {start_ms}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn export_at_offset_10s_maps_to_chapter_offset_when_no_head_trim() {
        // No leading silence → DeckLoad at 10s should land at 10_000 ms.
        let root = scratch_root("chap-notrim");
        let sid = "20260518T000002Z-cccc";
        seed_session(&root, sid, 0.0, 22.0, 0.0, &[(10.0, "anthem")]);
        let out = root.join("export.wav");
        let summary = export_session_in(&root, sid, &out).expect("export ok");
        assert_eq!(summary.chapter_count, 1);
        assert!(summary.trimmed_head_s < 0.6, "expected ~0 head trim");
        let sidecar = chapters_sidecar_path(&out);
        let text = fs::read_to_string(&sidecar).unwrap();
        let start_line = text.lines().find(|l| l.starts_with("START=")).unwrap();
        let start_ms: i64 = start_line.trim_start_matches("START=").parse().unwrap();
        assert!(
            (9500..=10500).contains(&start_ms),
            "expected ~10000 ms, got {start_ms}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn export_empty_session_returns_safe_error() {
        let root = scratch_root("empty");
        let sid = "20260518T000003Z-dddd";
        let dir = root.join(sid);
        fs::create_dir_all(&dir).unwrap();
        // No master.wav at all.
        let out = root.join("nope.wav");
        let err = export_session_in(&root, sid, &out).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("master.wav missing"),
            "unexpected error: {msg}"
        );
        // All-silent master.wav → must fail loudly, not write an empty wav.
        let mut f = File::create(dir.join(crate::persistence::sessions::MASTER_WAV_FILENAME))
            .expect("create wav");
        let data_bytes = SR * 5 * 2 * 4; // 5s silence
        write_wav_header(&mut f, SR, data_bytes).expect("header");
        let zeros = vec![0u8; data_bytes as usize];
        f.write_all(&zeros).unwrap();
        drop(f);
        let err = export_session_in(&root, sid, &out).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("silent end-to-end"), "unexpected error: {msg}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn export_with_no_events_log_writes_empty_chapter_sidecar() {
        let root = scratch_root("no-events");
        let sid = "20260518T000004Z-eeee";
        let dir = root.join(sid);
        fs::create_dir_all(&dir).unwrap();
        write_test_wav(
            &dir.join(crate::persistence::sessions::MASTER_WAV_FILENAME),
            1.0,
            10.0,
            1.0,
        );
        // No events.jsonl written.
        let out = root.join("e.wav");
        let summary = export_session_in(&root, sid, &out).expect("export ok");
        assert_eq!(summary.chapter_count, 0);
        let sidecar = chapters_sidecar_path(&out);
        let text = fs::read_to_string(&sidecar).unwrap();
        assert!(text.starts_with(";FFMETADATA1"));
        assert!(!text.contains("[CHAPTER]"), "no chapters expected");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn export_rejects_path_traversal_in_session_id() {
        let root = scratch_root("traversal");
        let out = root.join("e.wav");
        let err = export_session_in(&root, "../etc", &out).expect_err("must reject");
        assert!(format!("{err:#}").contains("invalid session id"));
        let err = export_session_in(&root, "foo/bar", &out).expect_err("must reject");
        assert!(format!("{err:#}").contains("invalid session id"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn export_multiple_deckloads_become_ordered_chapters_with_end_times() {
        let root = scratch_root("multi");
        let sid = "20260518T000005Z-ffff";
        // 0s head, 30s tone, 0s tail. DeckLoad at 0s, 10s, 20s.
        seed_session(
            &root,
            sid,
            0.0,
            30.0,
            0.0,
            &[(0.0, "t1"), (10.0, "t2"), (20.0, "t3")],
        );
        let out = root.join("e.wav");
        let summary = export_session_in(&root, sid, &out).expect("export ok");
        assert_eq!(summary.chapter_count, 3);
        let sidecar = chapters_sidecar_path(&out);
        let text = fs::read_to_string(&sidecar).unwrap();
        // Ordered START values (0, ~10000, ~20000) and matching END values.
        let starts: Vec<i64> = text
            .lines()
            .filter_map(|l| l.strip_prefix("START="))
            .filter_map(|s| s.parse().ok())
            .collect();
        assert_eq!(starts.len(), 3);
        assert!(starts[0] < starts[1] && starts[1] < starts[2]);
        // Titles preserve order.
        let titles: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("title="))
            .collect();
        assert_eq!(titles, vec!["t1", "t2", "t3"]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn detect_silence_finds_audible_window_in_short_buffer() {
        // < silence-window length: detector must NOT trim everything.
        let mut buf: Vec<f32> = Vec::with_capacity(SR as usize);
        for i in 0..SR as usize {
            let t = i as f32 / SR as f32;
            let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            buf.push(s);
            buf.push(s);
        }
        let (head, tail) = detect_silence_trim(&buf, SR);
        // Short buffer falls into the "shorter than one silence window"
        // branch → no trim.
        assert_eq!(head, 0);
        assert_eq!(tail, 0);
    }

    #[test]
    fn chapters_sidecar_path_appends_dot_chapters_txt() {
        let p = chapters_sidecar_path(Path::new("/tmp/out.wav"));
        assert_eq!(p, PathBuf::from("/tmp/out.wav.chapters.txt"));
    }

    #[test]
    fn deckload_past_trimmed_tail_is_dropped_from_chapters() {
        // Tone is only 10 s. DeckLoad at 25 s falls past the output → drop.
        let root = scratch_root("past-tail");
        let sid = "20260518T000007Z-gggg";
        seed_session(&root, sid, 0.0, 10.0, 0.0, &[(0.0, "t1"), (25.0, "stale")]);
        let out = root.join("e.wav");
        let summary = export_session_in(&root, sid, &out).expect("export ok");
        assert_eq!(summary.chapter_count, 1, "stale chapter must be dropped");
        let sidecar = chapters_sidecar_path(&out);
        let text = fs::read_to_string(&sidecar).unwrap();
        assert!(text.contains("title=t1"));
        assert!(!text.contains("title=stale"));
        fs::remove_dir_all(&root).ok();
    }
}
