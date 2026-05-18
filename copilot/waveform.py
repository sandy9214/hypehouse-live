"""Waveform peak-pair computation for UI rendering.

The UI's :component:`Waveform` canvas can't render full-resolution PCM
(a 5-minute 44.1 kHz mono track is ~13 M samples — too much to ship
over the WebSocket and too much for the canvas to draw). The industry
pattern is min/max **peak pairs**: bin the decoded waveform into N
buckets and store the (min, max) per bucket. At ``target_samples=2000``
this is 4 KB per track — tiny enough to embed in the SQLite catalog and
ship verbatim to the browser.

Wire shape:

    [ min_0, max_0, min_1, max_1, ..., min_{N-1}, max_{N-1} ]

Each value is an :class:`int` 8-bit signed (``-128..=127``) mapping the
PCM range ``-1.0..=1.0``. ``target_samples * 2`` bytes total.

Why ``i8`` and not ``i16``: a 4000-pixel canvas at 1px per peak only
needs ~7 bits of vertical resolution to look indistinguishable from
full-float draws. Halving the byte budget keeps the WS payload + SQLite
column small enough to ship inline rather than via a separate object
store.

This module is import-cheap (numpy only at call time) so callers that
just want to *read* a stored ``waveform_peaks`` BLOB don't pay the
librosa cold-start tax.
"""
from __future__ import annotations

from pathlib import Path

# Default bucket count — 2000 peak pairs renders cleanly on a 4000 px
# canvas (2 px per pair). Smaller produces visible aliasing at typical
# deck-strip widths; larger blows the WS payload past what the
# JSON-RPC envelope wants without measurable visual gain.
DEFAULT_TARGET_SAMPLES = 2000

# Each bucket emits two i8 bytes (min, max). Keeping this as a module
# constant lets callers compute expected BLOB sizes without rebuilding
# the formula at every callsite.
BYTES_PER_BUCKET = 2

# Signed 8-bit bounds. Pulled out as constants so the float→int8 mapping
# below is readable.
_I8_MAX = 127
_I8_MIN = -128


def compute_peaks(
    audio_path: Path,
    target_samples: int = DEFAULT_TARGET_SAMPLES,
) -> bytes:
    """Compute min/max peak pairs from an audio file.

    Loads the audio at 22050 Hz mono via librosa, bins it into
    ``target_samples`` equal-width buckets, takes per-bucket (min, max),
    quantizes each to ``i8`` (range ``-128..=127``) and packs into a
    contiguous ``bytes`` payload.

    Args:
        audio_path: Path to a decodable audio file (mp3 / wav / flac / m4a / aac / ogg).
        target_samples: Bucket count. Defaults to :data:`DEFAULT_TARGET_SAMPLES`.
            Clamped to ``[16, 16384]`` — under 16 produces unrenderable
            jagged output, over 16k blows past sensible BLOB sizes.

    Returns:
        Packed ``bytes`` of length ``target_samples * 2``. The first
        byte is ``min_0``, the second ``max_0``, then ``min_1``,
        ``max_1``, etc.

    Raises:
        FileNotFoundError: ``audio_path`` does not exist.
        ValueError: audio decode produced zero samples (silent /
            truncated / unsupported codec).
    """
    if not audio_path.exists():
        raise FileNotFoundError(f"audio file not found: {audio_path}")

    # Lazy librosa import — pulls numba which takes ~3s. Callers that
    # only read stored peaks shouldn't pay this.
    import librosa  # type: ignore[import-not-found]

    target_samples = max(16, min(int(target_samples), 16384))

    # Mono load at 22 kHz: sufficient resolution for visual peaks, an
    # order of magnitude smaller than full-rate stereo. We load the
    # *entire* track (no duration limit) — visual peaks must cover the
    # whole timeline, unlike the analyzer's 90/150s window for BPM.
    y, _sr = librosa.load(str(audio_path), sr=22050, mono=True)
    return _peaks_from_samples(y, target_samples)


def _peaks_from_samples(
    samples: object,  # numpy.ndarray — typed loosely so tests skip numpy import here
    target_samples: int,
) -> bytes:
    """Pure-numpy peak-pair packer. Split out so tests can feed
    synthetic arrays without hitting librosa.
    """
    import numpy as np

    arr = np.asarray(samples, dtype=np.float32)
    n = int(arr.size)
    if n == 0:
        raise ValueError("audio decoded to zero samples")

    target_samples = max(16, min(int(target_samples), 16384))

    # If the audio is shorter than target_samples (very short clip),
    # pad with zeros on the right so every bucket still has at least
    # one sample. Avoids a divide-by-zero in the bucket-edge math when
    # the bucket would otherwise be empty.
    if n < target_samples:
        pad = np.zeros(target_samples - n, dtype=np.float32)
        arr = np.concatenate([arr, pad])
        n = target_samples

    # Bucket assignment via index math is ~10× faster than np.array_split
    # for typical track sizes (5-minute track = ~6.6M samples → 2k buckets
    # = ~3.3k samples per bucket). We round to int boundaries so every
    # sample lands in exactly one bucket.
    edges = np.linspace(0, n, target_samples + 1, dtype=np.int64)

    out = bytearray(target_samples * BYTES_PER_BUCKET)
    for i in range(target_samples):
        lo = int(edges[i])
        hi = int(edges[i + 1])
        if hi <= lo:
            # Degenerate (shouldn't happen post-pad) — emit a zero pair.
            out[i * 2] = 0
            out[i * 2 + 1] = 0
            continue
        bucket = arr[lo:hi]
        mn = float(bucket.min())
        mx = float(bucket.max())
        out[i * 2] = _f32_to_i8(mn) & 0xFF
        out[i * 2 + 1] = _f32_to_i8(mx) & 0xFF
    return bytes(out)


def _f32_to_i8(v: float) -> int:
    """Map a -1.0..=1.0 float to an -128..=127 int.

    Out-of-range floats (clipped audio, fixed-point quirks) clamp to
    the i8 bounds rather than overflowing into garbage values. Returned
    as a Python int in the signed range — callers ``& 0xFF`` to pack
    into a single byte.
    """
    # Multiply by 127 then round — keeps 0.0 → 0 exact and saturates at
    # the boundary. Using 128 for the negative side would push -1.0 to
    # -128 (representable) but +1.0 to +128 (not representable in i8),
    # so 127 is the symmetric choice.
    scaled = int(round(max(-1.0, min(1.0, v)) * _I8_MAX))
    return max(_I8_MIN, min(_I8_MAX, scaled))


def expected_bytes(target_samples: int) -> int:
    """Return the byte length :func:`compute_peaks` produces for a
    given bucket count. Helpful for tests + callers building a BLOB
    column with a CHECK constraint."""
    target_samples = max(16, min(int(target_samples), 16384))
    return target_samples * BYTES_PER_BUCKET
