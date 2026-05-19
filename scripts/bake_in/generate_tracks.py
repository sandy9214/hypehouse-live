"""Synthesize a deterministic catalog of test WAVs for the bake-in harness.

Each track is a stereo PCM16 WAV at 44.1 kHz containing a 440 Hz sine
tone summed with a four-on-the-floor kick at the chosen BPM. The output
is intentionally crude — the goal is to exercise the engine's decoder /
beat-grid / auto-mix path with material that has:

* A deterministic, detectable BPM (so the analyzer's tempo estimate
  matches what we asked for, give or take ±2).
* A reasonable peak level (≈ -3 dBFS) so the loudness leveler has a
  non-degenerate signal to work with.
* A short duration by default (30 seconds × 50 tracks ≈ 25 min worth of
  back-to-back playback) so the v0.2 "sanity" bake-in fits in a laptop
  CI run. The ``--duration-min`` knob bumps every track's length so the
  total catalog adds up to the requested wall-clock window — used for
  the 240-minute soak the v0.2 release checklist demands.

Only the Python stdlib is required. The script is a plain CLI so the
Makefile + bake-in workflow can drive it with a single argv vector.

Wire shape of the per-run manifest dumped at ``<out>/manifest.json``:

.. code-block:: json

    {
      "schema": 1,
      "sample_rate": 44100,
      "channels": 2,
      "track_duration_s": 30.0,
      "total_duration_s": 1500.0,
      "tracks": [
        {"id": "synth-001", "path": "/tmp/.../tracks/synth-001.wav",
         "bpm": 124, "camelot_key": "8A"}, ...
      ]
    }

Downstream (``scripts.bake_in.run_set``) reads the manifest so it can
seed the copilot library and assert post-run that the engine consumed
the expected catalog.
"""
from __future__ import annotations

import argparse
import json
import logging
import math
import random
import struct
import sys
import wave
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Sequence

log = logging.getLogger("bake_in.generate_tracks")

# --- knobs ----------------------------------------------------------------

#: Output sample rate. 44.1 kHz keeps the WAVs decodable by every codec
#: in the engine's symphonia matrix without resampling overhead.
SAMPLE_RATE_HZ: int = 44_100

#: Number of synthetic tracks per run. 50 × 30 s ≈ 25 min of catalog —
#: roughly one track every 30 s of transition cadence with a comfortable
#: queue depth past the playlist length the harness enqueues.
DEFAULT_TRACK_COUNT: int = 50

#: Default per-track duration (seconds). Bumped by ``--duration-min`` so
#: the catalog wall-clock matches the requested bake length.
DEFAULT_TRACK_SECONDS: float = 30.0

#: Default bake duration in minutes (v0 "sanity" run). The 240-minute
#: soak is opt-in via the ``--duration-min`` flag so a developer doesn't
#: accidentally fire a four-hour bake by typing ``make bake-in``.
DEFAULT_DURATION_MIN: float = 25.0

#: BPM picker range (uniform integer in [BPM_MIN, BPM_MAX]). The engine's
#: tempo-stretch path is healthiest in 85-145 BPM, the same band v1's
#: mashability scorer targets.
BPM_MIN: int = 85
BPM_MAX: int = 145

#: Sine tone frequency. A4 (440 Hz) sits comfortably between the kick's
#: fundamental and the engine's nyquist; ear-friendly when an operator
#: pipes the bake into headphones for spot-check.
SINE_HZ: float = 440.0

#: Target peak level (linear). -3 dBFS = 10 ** (-3 / 20) ≈ 0.708. Keeps
#: the leveler's headroom budget intact while leaving the limiter
#: something to engage on the loudest sample.
TARGET_PEAK_LINEAR: float = 0.7079

#: Camelot wheel — 12 keys × {A=minor, B=major} = 24 positions. The
#: order matches v1's analyzer output so the mashability scorer's
#: neighbour rules are exercised by the catalog.
CAMELOT_KEYS: tuple[str, ...] = tuple(
    f"{n}{suffix}" for n in range(1, 13) for suffix in ("A", "B")
)


@dataclass(frozen=True)
class TrackSpec:
    """One synthesized track's wire-visible metadata."""

    id: str
    path: str
    bpm: int
    camelot_key: str


def _kick_envelope(samples_per_beat: int, total: int) -> list[float]:
    """Return a kick-drum envelope of length ``total``.

    A four-on-the-floor kick is just an exponentially-decaying transient
    every ``samples_per_beat``. We approximate it as a sine burst at
    ``KICK_HZ`` with a 30 ms decay — cheap and analyzer-friendly.
    """
    decay_samples = max(1, int(0.030 * SAMPLE_RATE_HZ))  # 30 ms
    kick_hz = 80.0  # sub-bass thump
    env: list[float] = [0.0] * total
    for beat_start in range(0, total, samples_per_beat):
        for n in range(min(decay_samples, total - beat_start)):
            amp = math.exp(-3.0 * n / decay_samples)
            env[beat_start + n] += amp * math.sin(
                2.0 * math.pi * kick_hz * (n / SAMPLE_RATE_HZ)
            )
    return env


def _peak_normalize(samples: list[float], target: float = TARGET_PEAK_LINEAR) -> None:
    """In-place peak-normalize ``samples`` to ``target`` (linear, 0-1).

    Operates in place so the harness doesn't allocate a second buffer
    per track; the synthesizer already holds the only reference.
    """
    peak = max((abs(s) for s in samples), default=1.0)
    if peak <= 1e-9:
        return
    scale = target / peak
    for i, s in enumerate(samples):
        samples[i] = s * scale


def synthesize_track_samples(bpm: int, duration_s: float) -> list[float]:
    """Build the mono PCM-float sample list for one synthetic track.

    The mix is a sine bed + four-on-the-floor kick. The function returns
    a Python list because the bake-in tracks are short (30 s default)
    and numpy is intentionally not a hard dep of the harness — keeping
    the script numpy-free means the GH Actions runner doesn't pay the
    100 MB wheel cost just to bake the catalog.
    """
    if bpm <= 0:
        raise ValueError("bpm must be > 0")
    if duration_s <= 0:
        raise ValueError("duration_s must be > 0")
    total = int(duration_s * SAMPLE_RATE_HZ)
    samples_per_beat = max(1, int(SAMPLE_RATE_HZ * 60.0 / bpm))
    # Sine bed at 440 Hz.
    out: list[float] = [
        0.5 * math.sin(2.0 * math.pi * SINE_HZ * (n / SAMPLE_RATE_HZ))
        for n in range(total)
    ]
    kick = _kick_envelope(samples_per_beat, total)
    for n in range(total):
        out[n] += kick[n]
    _peak_normalize(out)
    return out


def write_wav_stereo(path: Path, mono: list[float]) -> None:
    """Write ``mono`` as a 16-bit stereo WAV (duplicated channels).

    Stereo is required by the engine — the cpal sink runs interleaved
    f32 stereo and the symphonia decoder routes mono into a panned
    duplicate, but a synthetic mono input would trip the decoder's
    "unsupported channel layout" path on some builds. Keeping the
    bake-in catalog explicitly stereo eliminates that variable.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(2)
        wf.setsampwidth(2)  # PCM16
        wf.setframerate(SAMPLE_RATE_HZ)
        # Interleaved L+R as PCM16 little-endian. Clamp + scale once;
        # struct.pack is the bottleneck so we feed it the whole array
        # in a single call per chunk to amortize the call overhead.
        chunk = 8192
        max_int16 = 32_767
        for i in range(0, len(mono), chunk):
            block = mono[i : i + chunk]
            # Build the interleaved frame buffer as ints first so the
            # pack call only walks the data once.
            ints: list[int] = []
            for s in block:
                v = max(-1.0, min(1.0, s))
                pcm = int(v * max_int16)
                ints.append(pcm)
                ints.append(pcm)  # duplicate L → R
            wf.writeframes(struct.pack(f"<{len(ints)}h", *ints))


def plan_catalog(
    count: int,
    duration_min: float,
    seed: int,
) -> tuple[float, list[tuple[str, int, str]]]:
    """Pick BPMs + Camelot keys for the catalog.

    Returns ``(per_track_seconds, [(id, bpm, key), ...])``. Per-track
    seconds is derived from ``duration_min`` so the catalog wall-clock
    matches the requested bake window — i.e. a 240-minute bake with 50
    tracks gives 288 s tracks rather than synthesizing 480 of them.
    """
    if count <= 0:
        raise ValueError("count must be > 0")
    if duration_min <= 0:
        raise ValueError("duration_min must be > 0")
    rng = random.Random(seed)
    per_track_s = max(DEFAULT_TRACK_SECONDS, (duration_min * 60.0) / count)
    plan: list[tuple[str, int, str]] = []
    for i in range(count):
        track_id = f"synth-{i + 1:03d}"
        bpm = rng.randint(BPM_MIN, BPM_MAX)
        key = rng.choice(CAMELOT_KEYS)
        plan.append((track_id, bpm, key))
    return per_track_s, plan


def write_manifest(out_dir: Path, specs: Sequence[TrackSpec], per_track_s: float) -> Path:
    """Persist the manifest so ``run_set`` can replay the catalog."""
    manifest = {
        "schema": 1,
        "sample_rate": SAMPLE_RATE_HZ,
        "channels": 2,
        "track_duration_s": float(per_track_s),
        "total_duration_s": float(per_track_s * len(specs)),
        "tracks": [asdict(s) for s in specs],
    }
    path = out_dir / "manifest.json"
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True))
    return path


def generate(
    out_dir: Path,
    count: int = DEFAULT_TRACK_COUNT,
    duration_min: float = DEFAULT_DURATION_MIN,
    seed: int = 0xBA5E,
) -> Path:
    """Generate the catalog under ``out_dir/tracks/``; return manifest path."""
    tracks_dir = out_dir / "tracks"
    tracks_dir.mkdir(parents=True, exist_ok=True)
    per_track_s, plan = plan_catalog(count, duration_min, seed)
    specs: list[TrackSpec] = []
    for track_id, bpm, key in plan:
        wav_path = tracks_dir / f"{track_id}.wav"
        samples = synthesize_track_samples(bpm, per_track_s)
        write_wav_stereo(wav_path, samples)
        specs.append(
            TrackSpec(id=track_id, path=str(wav_path), bpm=bpm, camelot_key=key)
        )
        log.debug("synthesized %s bpm=%d key=%s", wav_path.name, bpm, key)
    manifest = write_manifest(out_dir, specs, per_track_s)
    log.info(
        "wrote %d tracks (%.1f s each) → %s", len(specs), per_track_s, tracks_dir
    )
    log.info("manifest: %s", manifest)
    return manifest


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="bake_in.generate_tracks",
        description="Synthesize WAV catalog for the bake-in harness.",
    )
    p.add_argument(
        "--out-dir",
        type=Path,
        required=True,
        help="output directory; WAVs land under <out>/tracks/.",
    )
    p.add_argument(
        "--count",
        type=int,
        default=DEFAULT_TRACK_COUNT,
        help=f"number of tracks to synthesize (default {DEFAULT_TRACK_COUNT}).",
    )
    p.add_argument(
        "--duration-min",
        type=float,
        default=DEFAULT_DURATION_MIN,
        help=(
            "target total catalog wall-clock minutes; per-track length "
            "is derived so count × per_track ≈ duration_min. Default "
            f"{DEFAULT_DURATION_MIN:g} (the v0 sanity bake)."
        ),
    )
    p.add_argument(
        "--seed",
        type=int,
        default=0xBA5E,
        help="RNG seed for BPM/key picks (default 0xBA5E for reproducibility).",
    )
    p.add_argument(
        "--log-level",
        default="INFO",
        help="logging verbosity (DEBUG/INFO/WARNING/ERROR).",
    )
    return p


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    logging.basicConfig(
        level=args.log_level.upper(),
        format="%(asctime)s %(name)s %(levelname)s %(message)s",
    )
    generate(
        out_dir=args.out_dir,
        count=args.count,
        duration_min=args.duration_min,
        seed=args.seed,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
