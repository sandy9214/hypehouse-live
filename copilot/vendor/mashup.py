"""Smart mashup: demucs stems + structural overlay.

Strategy (2+ tracks):
  - Separate every track into vocals + instrumental via demucs htdemucs (2-stems).
  - Pick 'donor' (vocals source) = odd-indexed track; 'bed' (instrumental source) = even.
  - Find chorus of donor via vocals-RMS peak (in a 16-bar window).
  - Find matching-length instrumental section of bed avoiding its own vocal peaks.
  - Time-stretch donor vocals to bed's BPM and beat-align.
  - Overlay vocals on instrumental + tail of bed outro + intro of bed = mashup.
"""
from __future__ import annotations

import shutil
import subprocess
from collections.abc import Callable
from pathlib import Path

import librosa
import numpy as np
import pyrubberband as pyrb
import soundfile as sf

from .analyzer import TrackAnalysis
from .mixer import BEATS_PER_BAR, SR, _crossfade_segment, _loudness_match

Log = Callable[[str], None]
MASHUP_SECTION_BARS = 16


def _demucs_stems(wav_path: Path, cache_dir: Path, log: Log) -> tuple[Path, Path]:
    """Run demucs 2-stem (vocals/no_vocals). Returns (vocals_path, instrumental_path)."""
    stems_dir = cache_dir / "stems" / wav_path.stem
    vocals = stems_dir / "vocals.wav"
    instr = stems_dir / "no_vocals.wav"
    if vocals.exists() and instr.exists():
        log(f"  stems cached: {wav_path.stem}")
        return vocals, instr

    log(f"  separating stems: {wav_path.name} (~60s)")
    out_root = cache_dir / "stems_work"
    out_root.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["python", "-m", "demucs", "--two-stems=vocals",
         "-n", "htdemucs", "-o", str(out_root), str(wav_path)],
        check=True, timeout=600,
    )
    produced = out_root / "htdemucs" / wav_path.stem
    stems_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(produced / "vocals.wav", vocals)
    shutil.copy2(produced / "no_vocals.wav", instr)
    shutil.rmtree(produced, ignore_errors=True)
    return vocals, instr


def _load(path: Path) -> np.ndarray:
    y, _ = librosa.load(str(path), sr=SR, mono=False)
    if y.ndim == 1:
        y = np.stack([y, y])
    return y


def _stretch(y: np.ndarray, rate: float) -> np.ndarray:
    if abs(rate - 1.0) < 0.005:
        return y
    return np.stack([pyrb.time_stretch(y[c], SR, rate) for c in range(y.shape[0])])


def _find_chorus(vocals: np.ndarray, bpm: float, bars: int = MASHUP_SECTION_BARS) -> int:
    """Return sample index of vocal-RMS peak over `bars`-length window."""
    mono = vocals.mean(axis=0) if vocals.ndim == 2 else vocals
    win_sec = (60.0 / bpm) * BEATS_PER_BAR * bars
    win = int(win_sec * SR)
    if win >= len(mono):
        return 0
    # Sliding RMS on hop=1s
    hop = SR
    rms = []
    for start in range(0, len(mono) - win, hop):
        seg = mono[start:start + win]
        rms.append((start, float(np.sqrt(np.mean(seg ** 2)))))
    if not rms:
        return 0
    rms.sort(key=lambda x: x[1], reverse=True)
    return rms[0][0]


def _find_quiet_vox_section(vocals: np.ndarray, bpm: float, bars: int = MASHUP_SECTION_BARS) -> int:
    """Opposite: section with LEAST vocals (best for overlaying donor vocals)."""
    mono = vocals.mean(axis=0) if vocals.ndim == 2 else vocals
    win_sec = (60.0 / bpm) * BEATS_PER_BAR * bars
    win = int(win_sec * SR)
    if win >= len(mono):
        return 0
    hop = SR
    rms = []
    for start in range(0, len(mono) - win, hop):
        seg = mono[start:start + win]
        rms.append((start, float(np.sqrt(np.mean(seg ** 2)))))
    if not rms:
        return 0
    # Skip first 8 bars (intro) and last 8 bars (outro)
    intro_samples = int((60.0 / bpm) * BEATS_PER_BAR * 8 * SR)
    outro_samples = intro_samples
    rms = [(s, r) for s, r in rms if s > intro_samples and s < len(mono) - win - outro_samples]
    if not rms:
        return intro_samples
    rms.sort(key=lambda x: x[1])
    return rms[0][0]


def _snap_to_beat(sample: int, bpm: float) -> int:
    spb = SR * 60.0 / bpm
    return int(round(sample / spb) * spb)


def smart_mashup(tracks: list[TrackAnalysis], output_path: Path, cache_dir: Path, log: Log) -> Path:
    """Build mashup from N tracks. Alternates donor/bed role for variety."""
    if len(tracks) < 2:
        # Raise unified exception so web.py handles with product message,
        # not a backend ValueError text leaked to the user.
        from .pipeline import InsufficientMixableTracks
        raise InsufficientMixableTracks(fetched=len(tracks), analyzed=len(tracks))

    log(f"[mashup] {len(tracks)} tracks")
    tgt_bpm = float(np.median([t.bpm for t in tracks]))
    log(f"[mashup] target BPM: {tgt_bpm:.1f}")

    # Separate stems for every track
    stems: list[tuple[np.ndarray, np.ndarray, float]] = []  # (vocals, instrumental, original_bpm)
    for t in tracks:
        vp, ip = _demucs_stems(Path(t.path), cache_dir, log)
        vocals = _load(vp)
        instr = _load(ip)
        rate = tgt_bpm / t.bpm
        vocals = _stretch(vocals, rate)
        instr = _stretch(instr, rate)
        stems.append((vocals, instr, t.bpm))

    section_sec = (60.0 / tgt_bpm) * BEATS_PER_BAR * MASHUP_SECTION_BARS
    section_n = int(section_sec * SR)
    log(f"[mashup] section length: {section_sec:.1f}s ({MASHUP_SECTION_BARS} bars)")

    # Build sequence: for each pair (i, i+1), produce segment = bed intro + mashup core + bed outro
    output_segments: list[np.ndarray] = []
    transitions: list[float] = []
    cursor_sec = 0.0

    for i in range(len(stems)):
        donor_v, _donor_i, _ = stems[i]
        bed_v, bed_i, _ = stems[(i + 1) % len(stems)]

        # Find donor chorus (peak vocals) and snap to beat
        donor_start = _snap_to_beat(_find_chorus(donor_v, tgt_bpm), tgt_bpm)
        donor_end = min(donor_start + section_n, donor_v.shape[1])
        donor_chunk_vox = donor_v[:, donor_start:donor_end]

        # Find quiet-vocals section of bed
        bed_start = _snap_to_beat(_find_quiet_vox_section(bed_v, tgt_bpm), tgt_bpm)
        bed_end = min(bed_start + section_n, bed_i.shape[1])
        bed_chunk_instr = bed_i[:, bed_start:bed_end]

        n = min(donor_chunk_vox.shape[1], bed_chunk_instr.shape[1])
        donor_chunk_vox = _loudness_match(donor_chunk_vox[:, :n], target=0.15)
        bed_chunk_instr = _loudness_match(bed_chunk_instr[:, :n], target=0.18)

        mashup = donor_chunk_vox + bed_chunk_instr
        output_segments.append(mashup)
        transitions.append(cursor_sec)
        cursor_sec += n / SR

    # Concatenate with short crossfades between segments
    xfade_n = int((60.0 / tgt_bpm) * BEATS_PER_BAR * 2 * SR)  # 2-bar xfade
    out = output_segments[0]
    for seg in output_segments[1:]:
        n = min(xfade_n, out.shape[1], seg.shape[1])
        if n < 1000:
            out = np.concatenate([out, seg], axis=1)
            continue
        a_tail = out[:, -n:]
        b_head = seg[:, :n]
        xf = _crossfade_segment(a_tail, b_head)
        out = np.concatenate([out[:, :-n], xf, seg[:, n:]], axis=1)

    # Fade in/out edges
    fn = min(SR * 2, out.shape[1])
    out[:, :fn] *= np.linspace(0, 1, fn)
    out[:, -fn:] *= np.linspace(1, 0, fn)

    # Final loudness
    peak = np.max(np.abs(out))
    if peak > 0:
        out = out / peak * 0.94
    out = np.tanh(out * 1.1) * 0.98

    output_path.parent.mkdir(parents=True, exist_ok=True)
    if output_path.suffix.lower() == ".mp3":
        wav_tmp = output_path.with_suffix(".wav")
        sf.write(str(wav_tmp), out.T, SR)
        subprocess.run(
            ["ffmpeg", "-y", "-loglevel", "error", "-i", str(wav_tmp),
             "-codec:a", "libmp3lame", "-b:a", "192k", str(output_path)],
            check=True, timeout=180,
        )
        wav_tmp.unlink(missing_ok=True)
    else:
        sf.write(str(output_path), out.T, SR)

    # Issue #108 (council HH-117): don't mutate module global TRANSITIONS.
    # Caller reads `transitions` from the return dict — concurrent renders
    # previously raced and corrupted the shared list.
    log(f"[mashup] done: {output_path.name} ({out.shape[1]/SR:.1f}s)")
    return {
        "output": output_path,
        "transitions": list(transitions),
        "target_bpm": float(tgt_bpm),
        "segments": [],
    }
