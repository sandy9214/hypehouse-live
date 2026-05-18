"""Integrated loudness measurement (ITU-R BS.1770 / EBU R128).

The DJ co-pilot ingests tracks from many sources (Spotify rips, vinyl
needle-drops, soundcloud bootlegs, lossless masters off Bandcamp) — and
every one of them has been mastered to a different reference level.
When a DJ swings the crossfader from a -23 LUFS jazz cut to a -8 LUFS
EDM banger, the audience is hit with a sudden +15 dB jump. This module
fixes that surprise.

For each track, we compute the **integrated loudness** (the single LUFS
value the streaming platforms publish on their analyse pass — Spotify
uses -14 LUFS, YouTube -14, Apple Music -16, Tidal -14) and store it in
the library. Combined with a target level (`-14 LUFS` by convention),
the per-track `track_gain_db` is just:

    track_gain_db = target_lufs - measured_lufs   (positive = boost)

The engine applies that gain post-decode + pre-effects, so the mix bus
sees every track at roughly the same level regardless of source master.

Why ITU-R BS.1770 (via pyloudnorm) rather than a naive RMS:

* BS.1770 K-weights the signal (broad pre-emphasis curve modelled on
  human loudness perception). A track with heavy sub-bass scores
  *lower* than a track of the same RMS dominated by upper-mids — the
  way listeners actually hear it.
* It uses a 400 ms gating window with a -10 LU relative gate; quiet
  passages don't pull the average down.
* It's the same algorithm the streaming platforms run, so the values
  we measure here are directly comparable to the published "target"
  numbers.

Public API:

* :func:`compute_lufs` — measure a single audio file, return the
  integrated loudness in LUFS.
* :func:`gain_db_for_target` — turn a measured LUFS value into the
  matching gain (dB) to hit ``target_lufs``.

Both functions are pure: no caching, no side effects, no library
writes. The library layer wires them into ``add_track_from_path`` and
stores the result in the new ``lufs`` / ``track_gain_db`` columns
(schema v7).

Lazy imports: librosa + pyloudnorm are heavy (~5s combined cold-start
on a fresh interpreter). They're imported *inside* :func:`compute_lufs`
so a user that never touches the loudness path doesn't pay for the
import — same pattern :mod:`copilot.library` uses for the analyzer.
"""
from __future__ import annotations

import logging
from pathlib import Path

log = logging.getLogger(__name__)

# Streaming-platform reference (Spotify / YouTube / Tidal). Apple Music
# uses -16 LUFS but the difference is within typical metering error; we
# default to -14 because most user libraries are Spotify-shaped.
# Callers can override per-call via ``gain_db_for_target(target=...)``.
DEFAULT_TARGET_LUFS = -14.0

# Lower / upper sanity bounds for measured LUFS.
#
# * The BS.1770 spec returns ``-inf`` for digital silence (no audible
#   sample anywhere in the file). pyloudnorm surfaces this as a very
#   large negative number. We clamp the *gain* (not the LUFS itself —
#   so the library can record the true silence signal) to keep the
#   engine's per-deck gain multiplier sane.
# * Realistically-mastered audio sits in [-30, -5] LUFS. We allow a
#   wider [-70, +3] envelope to cover the long tail of weirdly-mastered
#   tracks without false-positive clamps; anything outside that is
#   almost certainly a measurement bug rather than a real signal.
LUFS_SILENCE_FLOOR = -70.0
LUFS_MAX_REASONABLE = 3.0

# Maximum absolute gain (dB) we'll ever apply per deck. A +14 dB boost
# on top of a streaming-shaped master leaves enough headroom for the
# limiter to catch transients; pushing further reliably clips. A -20 dB
# attenuation is enough for the loudest masters we've seen in the wild
# (~+6 LUFS — louder than reference; rare).
MAX_GAIN_DB = 14.0
MIN_GAIN_DB = -20.0


def compute_lufs(audio_path: Path | str) -> float:
    """Return the integrated loudness of ``audio_path`` in LUFS.

    Loads the file via librosa (so we inherit its codec coverage:
    mp3 / wav / flac / m4a / ogg) at the file's native sample rate
    because the BS.1770 K-weighting filter coefficients are
    sample-rate dependent and pyloudnorm picks the right curve at
    Meter construction time.

    Stereo files: pyloudnorm wants ``(samples, channels)``; librosa
    returns ``(channels, samples)`` for stereo loads. We transpose
    once before passing the buffer in.

    Errors:

    * ``ImportError`` if ``pyloudnorm`` or ``librosa`` isn't installed
      (lazy import — see module docstring).
    * Any decode error from librosa propagates verbatim; the library
      layer wraps it in a best-effort try/except so a single bad file
      doesn't abort a directory scan.

    Returns:
        Measured integrated LUFS. Silent / near-silent inputs return
        a very-negative number (typically ``-inf`` or close to
        :data:`LUFS_SILENCE_FLOOR`); callers should clamp at the
        gain-calculation step, not here, so the raw measurement is
        preserved in the library for diagnostics.
    """
    # Lazy import: librosa cold-starts ~3s (numba JIT), pyloudnorm
    # pulls scipy filter design ~1s. Module-level imports would push
    # ``import copilot.loudness`` over the same cliff.
    import librosa  # type: ignore[import-not-found]
    import pyloudnorm  # type: ignore[import-not-found]

    path = Path(audio_path)
    # ``mono=False`` so stereo files stay stereo — channel-correlation
    # affects the BS.1770 score (broadband-stereo content is louder
    # than dual-mono of the same per-channel level by ~3 LU). Forcing
    # mono would over-attenuate real stereo masters.
    samples, sr = librosa.load(str(path), sr=None, mono=False)

    # Normalize buffer shape to (frames, channels) — pyloudnorm's
    # contract. librosa returns:
    #   * 1-D (samples,) for mono
    #   * 2-D (channels, samples) for stereo
    # We want (samples,) untouched + (samples, channels) for stereo.
    if samples.ndim == 2:
        samples = samples.T

    meter = pyloudnorm.Meter(sr)  # picks K-weighting for the given sr
    loudness = meter.integrated_loudness(samples)
    return float(loudness)


def gain_db_for_target(
    measured_lufs: float,
    target_lufs: float = DEFAULT_TARGET_LUFS,
) -> float:
    """Compute the gain (dB) that brings ``measured_lufs`` to ``target_lufs``.

    Positive return = boost (track was quieter than target). Negative
    return = attenuation (track was louder than target).

    Clamps:

    * Silence-floor: a measured value below :data:`LUFS_SILENCE_FLOOR`
      (or non-finite, including ``-inf`` which pyloudnorm returns for
      digital silence) yields ``0.0`` — we don't want to apply a
      +999 dB boost to a silent file and explode the audio thread.
    * Hard gain ceiling at :data:`MAX_GAIN_DB` / :data:`MIN_GAIN_DB`
      so a single oddball measurement can't push the per-deck
      multiplier into a clipping or sub-noise-floor regime.

    Examples:

    >>> gain_db_for_target(-23.0)            # quiet jazz → +9 dB boost
    9.0
    >>> gain_db_for_target(-8.0)             # loud EDM   → -6 dB cut
    -6.0
    >>> gain_db_for_target(-14.0)            # at target  →  0 dB
    0.0
    >>> gain_db_for_target(float('-inf'))    # silence    →  0 dB (clamped)
    0.0
    """
    import math

    # Non-finite (including ``-inf`` from pyloudnorm on silence): bail
    # to 0 dB. The library stores the raw LUFS separately so the
    # silence signal isn't lost — we just refuse to multiply by it.
    if not math.isfinite(measured_lufs):
        return 0.0
    if measured_lufs < LUFS_SILENCE_FLOOR:
        return 0.0
    if measured_lufs > LUFS_MAX_REASONABLE:
        # Defensive: a pyloudnorm bug or a synthetic test signal
        # returning +20 LUFS should not produce a -34 dB attenuation
        # that mutes the deck. Cap the raw measurement before the
        # subtraction.
        measured_lufs = LUFS_MAX_REASONABLE

    raw = target_lufs - measured_lufs
    # Clamp to the deck-safe range. The engine's master-bus limiter
    # catches anything that slips through, but keeping the per-deck
    # multiplier bounded means the limiter doesn't have to work as
    # hard on every track.
    if raw > MAX_GAIN_DB:
        return MAX_GAIN_DB
    if raw < MIN_GAIN_DB:
        return MIN_GAIN_DB
    return float(raw)
