"""Analyze audio: BPM, key (Camelot), beats, energy."""
from __future__ import annotations

from dataclasses import asdict, dataclass
from pathlib import Path

import librosa
import numpy as np

from src.shared_cache import Cache, get_cache

# Krumhansl-Schmuckler key profiles
MAJOR_PROFILE = np.array([6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88])
MINOR_PROFILE = np.array([6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17])
NOTES = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]

# Camelot wheel: (note, mode) -> Camelot code
CAMELOT = {
    ("C", "maj"): "8B", ("G", "maj"): "9B", ("D", "maj"): "10B", ("A", "maj"): "11B",
    ("E", "maj"): "12B", ("B", "maj"): "1B", ("F#", "maj"): "2B", ("C#", "maj"): "3B",
    ("G#", "maj"): "4B", ("D#", "maj"): "5B", ("A#", "maj"): "6B", ("F", "maj"): "7B",
    ("A", "min"): "8A", ("E", "min"): "9A", ("B", "min"): "10A", ("F#", "min"): "11A",
    ("C#", "min"): "12A", ("G#", "min"): "1A", ("D#", "min"): "2A", ("A#", "min"): "3A",
    ("F", "min"): "4A", ("C", "min"): "5A", ("G", "min"): "6A", ("D", "min"): "7A",
}


@dataclass
class TrackAnalysis:
    path: str
    duration: float
    sr: int
    bpm: float
    beats: list[float]
    key: str
    mode: str
    camelot: str
    energy: float
    bpm_norm: float = 0.0
    onset_env: list[float] = None
    onset_hop_sec: float = 1.0
    downbeats: list[float] = None
    beat_source: str = "librosa"
    segments: list[float] = None   # seconds; structural section boundaries
    # DJ-intel additions (Phase 4)
    drop_times: list[float] = None       # seconds — detected drop / impact points
    buildup_starts: list[float] = None   # seconds — buildup zones feeding into drops
    energy_profile: dict = None          # {intro, mid, drop, outro, overall, curve: [(t, e)]}

    def to_dict(self) -> dict:
        return asdict(self)


def _madmom_beats(path: str) -> tuple[list[float], list[float], str] | None:
    """Return (beats_sec, downbeats_sec, source) via madmom. None if unavailable."""
    try:
        from madmom.features.downbeats import DBNDownBeatTrackingProcessor, RNNDownBeatProcessor
    except Exception:
        return None
    try:
        proc = RNNDownBeatProcessor()
        act = proc(path)
        dbn = DBNDownBeatTrackingProcessor(beats_per_bar=[3, 4], fps=100)
        beats_with_positions = dbn(act)  # (N, 2) — time, beat-index-in-bar
        if len(beats_with_positions) < 4:
            return None
        beats = beats_with_positions[:, 0].tolist()
        downbeats = [t for t, pos in beats_with_positions if int(pos) == 1]
        return beats, downbeats, "madmom"
    except Exception as exc:
        print(f"[analyzer] madmom failed, falling back to librosa: {exc}")
        return None


def _safe_tempo(tempo: float, default: float = 120.0) -> tuple[float, bool]:
    """Clamp non-finite or non-positive tempo to a safe default before it
    can propagate into normalize_bpm() (which loops on inf) or mixer.py
    BPM math (which divides by it and produces NaN-stretch). Returns
    (safe_bpm, used_fallback)."""
    import math as _m
    try:
        t = float(tempo)
    except (TypeError, ValueError):
        return default, True
    if not _m.isfinite(t) or t <= 0:
        return default, True
    return t, False


def normalize_bpm(bpm: float, lo: float = 80.0, hi: float = 160.0) -> float:
    """Fold BPM into [lo, hi) via doublings/halvings. Handles double/half-time detection."""
    if bpm <= 0:
        return bpm
    b = bpm
    while b < lo:
        b *= 2
    while b >= hi:
        b /= 2
    return b


def _detect_key(y: np.ndarray, sr: int) -> tuple[str, str, str]:
    chroma = librosa.feature.chroma_cqt(y=y, sr=sr)
    chroma_mean = chroma.mean(axis=1)
    chroma_mean = chroma_mean / (chroma_mean.sum() + 1e-9)

    scores = []
    for i in range(12):
        maj_corr = np.corrcoef(np.roll(MAJOR_PROFILE, i), chroma_mean)[0, 1]
        min_corr = np.corrcoef(np.roll(MINOR_PROFILE, i), chroma_mean)[0, 1]
        scores.append((maj_corr, NOTES[i], "maj"))
        scores.append((min_corr, NOTES[i], "min"))
    scores.sort(reverse=True, key=lambda x: x[0])
    _, note, mode = scores[0]
    return note, mode, CAMELOT.get((note, mode), "?")


def _detect_drops_and_energy(y, sr, beats: list[float], segments: list[float], load_dur: float):
    """DJ-intel: find impact/drop points + build per-section energy profile.

    Drop heuristic (multi-signal):
      - spectral flux spike (sudden broadband energy increase)
      - bass-band RMS jump (sub-80Hz energy)
      - onset strength peak
    Combine signals, pick local maxima separated by ≥8 beats.
    """
    import numpy as _np
    hop = 512
    try:
        # --- Signal 1: bass-band RMS (sub-80 Hz) over short frames
        stft = _np.abs(librosa.stft(y, n_fft=2048, hop_length=hop))
        freqs = librosa.fft_frequencies(sr=sr, n_fft=2048)
        bass_mask = freqs < 80
        bass_rms = _np.sqrt(_np.mean(stft[bass_mask] ** 2, axis=0) + 1e-12)

        # --- Signal 2: spectral flux (broadband onset novelty)
        flux = librosa.onset.onset_strength(y=y, sr=sr, hop_length=hop)

        # --- Signal 3: full-band RMS
        rms = librosa.feature.rms(y=y, hop_length=hop)[0]

        # Normalize + combine
        def _nrm(v):
            v = _np.asarray(v, dtype=_np.float32)
            lo, hi = _np.percentile(v, [5, 95])
            rng = max(1e-9, hi - lo)
            return _np.clip((v - lo) / rng, 0, 1)

        n = min(len(bass_rms), len(flux), len(rms))
        combined = 0.5 * _nrm(bass_rms[:n]) + 0.3 * _nrm(flux[:n]) + 0.2 * _nrm(rms[:n])

        # Pick peaks separated by ≥8 beats (rough bar-scale minimum)
        if len(beats) >= 2:
            beat_s = max(0.25, float(_np.median(_np.diff(beats))))
        else:
            beat_s = 0.5
        min_gap = int((8 * beat_s) * sr / hop)
        peaks = librosa.util.peak_pick(combined, pre_max=min_gap, post_max=min_gap,
                                       pre_avg=min_gap, post_avg=min_gap,
                                       delta=0.15, wait=min_gap)
        drop_times_arr = librosa.frames_to_time(peaks, sr=sr, hop_length=hop)
        # Filter: skip drops in the first 10s (usually intro artifacts) and last 5s
        drop_times = [float(t) for t in drop_times_arr if 10.0 <= t <= max(10.0, load_dur - 5.0)]
        # Limit to top-3 by combined score to avoid noise
        if len(drop_times) > 3:
            scored = [(t, combined[int(t * sr / hop)]) for t in drop_times]
            scored.sort(key=lambda x: -x[1])
            drop_times = sorted([t for t, _ in scored[:3]])

        # Buildup start = 8-16 bars before each drop (snap to nearest downbeat if available)
        buildup_starts = []
        buildup_dur = 16 * beat_s
        for d in drop_times:
            bs = max(0.0, d - buildup_dur)
            buildup_starts.append(float(bs))

        # --- Energy profile (intro / mid / drop / outro)
        overall = float(_np.sqrt(_np.mean(y ** 2)))
        # Time index array for rms
        rms_t = librosa.frames_to_time(_np.arange(len(rms)), sr=sr, hop_length=hop)
        def _avg_energy(t0: float, t1: float) -> float:
            mask = (rms_t >= t0) & (rms_t < t1)
            return float(_np.mean(rms[mask])) if mask.any() else 0.0

        intro_end = min(load_dur * 0.15, 30.0)
        outro_start = max(load_dur * 0.85, load_dur - 30.0)
        intro_e = _avg_energy(0, intro_end)
        mid_e = _avg_energy(intro_end, outro_start)
        outro_e = _avg_energy(outro_start, load_dur)
        drop_e = max([_avg_energy(d - 2.0, d + 8.0) for d in drop_times], default=mid_e)

        # Build coarse energy curve (sampled every 2s)
        curve = []
        for i in range(0, int(load_dur), 2):
            curve.append([i, round(_avg_energy(i, i + 2), 4)])

        energy_profile = {
            "intro": round(intro_e, 4),
            "mid": round(mid_e, 4),
            "drop": round(drop_e, 4),
            "outro": round(outro_e, 4),
            "overall": round(overall, 4),
            # Normalized 0-100 score vs a typical pop-EDM max RMS (~0.3)
            "overall_score": round(min(100, overall / 0.3 * 100), 1),
            "curve": curve,
        }
        return drop_times, buildup_starts, energy_profile
    except Exception as exc:
        print(f"[analyzer] drop/energy detection failed: {exc}")
        return [], [], {"overall": 0.0, "curve": []}


def analyze(
    path: Path,
    cache_dir: Path,
    quick: bool = False,
    *,
    cache: Cache | None = None,
) -> TrackAnalysis:
    """If quick=True, skip madmom + structural segmentation (use librosa beats only).
    Saves ~20-25s per track at some mix-point accuracy cost.

    Cache I/O routes through the :class:`Cache` abstraction so analysis JSON
    can transparently move to a shared backend (e.g. GCS) and avoid every
    Cloud Run instance re-analyzing the same tracks. ``cache_dir`` remains
    for legacy callers + the LocalCache default; when ``cache`` is supplied
    it takes priority. Cache key follows the legacy on-disk shape so existing
    dev caches keep working without migration: ``<stem>.analysis.json``.
    """
    # Codex PR #289 P1: consult the env-driven factory instead of always
    # defaulting to LocalCache. CACHE_BACKEND=gcs now actually wires the
    # GcsCache into runtime callers.
    if cache is None:
        cache = get_cache(local_path=cache_dir)
    cache_key = f"{path.stem}.analysis.json"
    cached = cache.get_json(cache_key)
    if cached is not None:
        data = cached
        # Issue #31: poisoned caches written before _safe_tempo guard could
        # carry NaN / inf / 0 BPM. Run the guard on load so old caches
        # don't bypass it. Also guards bpm_norm symmetrically.
        cached_bpm, bpm_fallback = _safe_tempo(data.get("bpm", 120.0))
        if bpm_fallback:
            print(f"[analyzer] cache for {path.name}: poisoned bpm "
                  f"({data.get('bpm')!r}); clamped to {cached_bpm}")
            data["bpm"] = cached_bpm
            data["beat_source"] = (
                (data.get("beat_source") or "cached") + "+fallback_default_bpm"
            )
        if "bpm_norm" not in data or not data["bpm_norm"]:
            data["bpm_norm"] = normalize_bpm(cached_bpm)
        else:
            # Re-validate cached norm too — same poisoning vector. If the
            # cached norm itself is poisoned, recompute from the (already
            # sanitized) raw BPM instead of falling back to a flat 120 —
            # otherwise valid-raw + poisoned-norm produces wrong tempo
            # (e.g. raw=200 should become 100, not 120). Codex review.
            norm_guard, norm_fallback = _safe_tempo(data["bpm_norm"])
            data["bpm_norm"] = (
                normalize_bpm(cached_bpm) if norm_fallback else norm_guard
            )
        # Richness check: if we want full analysis but cache is from quick mode
        # (no madmom downbeats, no segments), re-analyze.
        has_downbeats = bool(data.get("downbeats"))
        has_segments = bool(data.get("segments"))
        if not quick and not (has_downbeats and has_segments):
            print(f"[analyzer] cache for {path.name} is quick-only, re-analyzing full")
        else:
            return TrackAnalysis(**data)

    # Load first 150s only — plenty for BPM/key/onset detection
    # Quick mode: shorter analysis window, no madmom, no segments
    load_dur = 90.0 if quick else 150.0
    y, sr = librosa.load(str(path), sr=22050, mono=True, duration=load_dur)
    # Try madmom for beats/downbeats (accurate), fall back to librosa
    mad = None if quick else _madmom_beats(str(path))
    if mad:
        beats, downbeats, beat_source = mad
        # Derive tempo from median inter-beat interval
        if len(beats) >= 2:
            ibi = np.diff(beats)
            tempo = 60.0 / float(np.median(ibi)) if len(ibi) else 120.0
        else:
            tempo = 120.0
    else:
        tempo, beat_frames = librosa.beat.beat_track(y=y, sr=sr)
        beats = librosa.frames_to_time(beat_frames, sr=sr).tolist()
        downbeats = beats[::4] if len(beats) >= 4 else beats
        beat_source = "librosa"
    key, mode, camelot = _detect_key(y, sr)
    energy = float(np.sqrt(np.mean(y ** 2)))
    onset_hop_sec = 1.0
    hop_length = int(sr * onset_hop_sec)
    oenv = librosa.onset.onset_strength(y=y, sr=sr, hop_length=hop_length)
    onset_env = oenv.tolist()

    # Structural segmentation — find section boundaries (intro/verse/chorus/etc transitions)
    # so we can place mix points at musically-meaningful moments.
    try:
        # Beat-synchronous chromagram at finer hop
        chroma_hop = 512
        chroma = librosa.feature.chroma_cqt(y=y, sr=sr, hop_length=chroma_hop)
        if len(beats) >= 6:
            beat_frames = librosa.time_to_frames(beats, sr=sr, hop_length=chroma_hop)
            beat_frames = beat_frames[beat_frames < chroma.shape[1]]
            sync_chroma = librosa.util.sync(chroma, beat_frames, aggregate=np.mean)
            # Target: one boundary per ~16 bars; segments ≈ ceil(num_beats / 64)
            n_seg = max(3, min(8, sync_chroma.shape[1] // 32))
            boundaries = librosa.segment.agglomerative(sync_chroma, n_seg)
            # Convert beat indices back to seconds
            segments = [float(beats[int(b)]) for b in boundaries if int(b) < len(beats)]
        else:
            segments = []
    except Exception as exc:
        print(f"[analyzer] segment detection failed: {exc}")
        segments = []

    # Drop detection + energy profile (Phase 4 DJ-intel)
    drop_times, buildup_starts, energy_profile = _detect_drops_and_energy(
        y, sr, beats, segments, load_dur=load_dur,
    )

    raw_bpm, bpm_fallback = _safe_tempo(tempo)
    if bpm_fallback:
        print(f"[analyzer] {path.name}: tempo non-finite/zero ({tempo!r}); "
              f"clamped to {raw_bpm} BPM. Track may be silent or beatless.")
        beat_source = f"{beat_source}+fallback_default_bpm"
    norm_bpm = normalize_bpm(raw_bpm)
    # True duration via header (without re-reading full audio)
    try:
        true_dur = float(librosa.get_duration(path=str(path)))
    except Exception:
        true_dur = float(librosa.get_duration(y=y, sr=sr))
    result = TrackAnalysis(
        path=str(path),
        duration=true_dur,
        sr=sr,
        bpm=raw_bpm,
        beats=beats,
        key=key,
        mode=mode,
        camelot=camelot,
        energy=energy,
        bpm_norm=norm_bpm,
        onset_env=onset_env,
        onset_hop_sec=onset_hop_sec,
        downbeats=downbeats,
        beat_source=beat_source,
        segments=segments,
        drop_times=drop_times,
        buildup_starts=buildup_starts,
        energy_profile=energy_profile,
    )
    # Issue #109 (council HH-118): atomic JSON write. LocalCache.put_json uses
    # the same tmp + os.replace pattern as PR #88's mp3 fix; GcsCache uses a
    # single PUT which is atomic at the GCS object level. Either way, parallel
    # analyze() on the same id can no longer leave a torn JSON on disk.
    cache.put_json(cache_key, result.to_dict())
    norm_note = f" (→{norm_bpm:.1f} norm)" if abs(raw_bpm - norm_bpm) > 1.0 else ""
    print(f"[analyzer] {path.name}: {raw_bpm:.1f} BPM{norm_note}, {camelot} ({key} {mode}) [{beat_source}]")
    return result
