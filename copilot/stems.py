"""Stem separation — split a track into vocals / drums / bass / other.

Stem-aware mashing wants to mix the *vocals* of one track over the
*drums* of another. To do that the engine needs per-track stem WAVs
sitting on disk so it can load them as independent decks instead of (or
in addition to) the full mix. This module is the **analyzer scaffold**
for that pipeline: it shells out to Facebook's `demucs` model and writes
``vocals.wav`` / ``drums.wav`` / ``bass.wav`` / ``other.wav`` into a
per-track cache directory.

Wire-up vs. exposure: this module ships the analyzer + library +
RPC plumbing (`library.compute_stems`, `library.get_stems`,
`library.stems_status`). The engine-side stem-deck audio path is
the `DeckLoadStems` event in `docs/api/ws-protocol.md`. The UI
AboutPanel "Stems" row (#197) surfaces aggregate ready/pending/
failed counts; the per-deck stem-mode toggle is a future v0.2
follow-up.

Heavy dependency caveat
-----------------------

`demucs` pulls in PyTorch (~2 GB wheel) **and** downloads a ~2 GB model
checkpoint on first invocation (cached under
``~/.cache/torch/hub/checkpoints/``). It is therefore an **optional**
dependency — `pip install hypehouse-copilot[stems]` opts in.

Timings (measured on `htdemucs`, 4-stem default):

* GPU (CUDA / Metal): ~30 s per 3-minute track.
* CPU: ~3 min per 3-minute track. Tolerable for offline ingest, far
  too slow for any real-time path.

Output cache shape
------------------

``<output_dir>/`` contains exactly four WAV files:

* ``vocals.wav``
* ``drums.wav``
* ``bass.wav``
* ``other.wav``

Each is stereo 44.1 kHz / 16-bit PCM (matching demucs's default writer
in `demucs.api.save_audio`). Approximate size budget: a 3-minute stereo
WAV at 44.1 kHz / 16-bit = ~31 MB; four of them = ~125 MB per track.
For a 200-track library that's ~25 GB of cache — operators should size
their stem cache disk accordingly.

Caching
-------

:func:`compute_stems` short-circuits when all four output WAVs are
already present and non-empty. Re-running stem separation is the
expensive part; a sub-millisecond stat check is the right tradeoff.
"""
from __future__ import annotations

import logging
from pathlib import Path

log = logging.getLogger(__name__)


# Canonical stem names. Order matches the demucs `htdemucs` model's
# output channel layout so a future engine integration can map directly.
STEM_NAMES: tuple[str, ...] = ("vocals", "drums", "bass", "other")

# Default model — `htdemucs` is demucs's hybrid transformer model and
# produces the 4-stem split that downstream mashing wants. We pin the
# string here rather than in callers so swapping models (e.g. to the
# 6-stem `htdemucs_6s`) happens in one place.
DEFAULT_MODEL = "htdemucs"


class StemsDependencyError(RuntimeError):
    """Raised when ``demucs`` (the optional dep) is not importable.

    Library + RPC layers translate this into a JSON-RPC ``-32000`` error
    with a "pip install hypehouse-copilot[stems]" hint. Keeping it a
    distinct subclass means callers don't have to string-match an
    ``ImportError`` to tell the install-hint case apart from a real
    crash inside demucs.
    """


def _cache_is_complete(output_dir: Path) -> dict[str, Path] | None:
    """Return the cached stem dict if all four WAVs exist + non-empty.

    Returns ``None`` if anything is missing. Cheap (4 stat calls); we
    call this before kicking off the heavy demucs invocation so a
    re-run on the same track is sub-millisecond.

    Zero-byte WAVs are treated as missing — a partial write from a
    killed previous run shouldn't masquerade as a valid cache hit.
    """
    if not output_dir.exists() or not output_dir.is_dir():
        return None
    out: dict[str, Path] = {}
    for name in STEM_NAMES:
        wav = output_dir / f"{name}.wav"
        if not wav.exists() or not wav.is_file():
            return None
        try:
            if wav.stat().st_size == 0:
                return None
        except OSError:
            return None
        out[name] = wav
    return out


def compute_stems(
    audio_path: Path,
    output_dir: Path,
    *,
    model: str = DEFAULT_MODEL,
) -> dict[str, Path]:
    """Run demucs on ``audio_path`` and write four stem WAVs into ``output_dir``.

    Args:
        audio_path: Decodable audio file (mp3 / wav / flac / m4a / aac /
            ogg). Passed through to demucs as a string path.
        output_dir: Per-track cache directory. Created if missing. The
            four output WAVs are written directly here (no
            ``demucs/<model>/<stem>.wav`` subtree — that's the demucs
            CLI's layout, not ours).
        model: Demucs model name. Defaults to :data:`DEFAULT_MODEL`
            (``htdemucs``). Override only if you've benchmarked a
            different model against your library.

    Returns:
        ``{"vocals": <Path>, "drums": <Path>, "bass": <Path>, "other": <Path>}``.
        Paths are absolute (since ``output_dir`` is resolved at call
        time by the library layer).

    Raises:
        FileNotFoundError: ``audio_path`` does not exist.
        StemsDependencyError: ``demucs`` is not installed.
        RuntimeError: demucs failed to produce one of the expected
            stems (model returned a different stem set, or a write
            failed silently).
    """
    if not audio_path.exists():
        raise FileNotFoundError(f"audio file not found: {audio_path}")

    output_dir.mkdir(parents=True, exist_ok=True)

    cached = _cache_is_complete(output_dir)
    if cached is not None:
        log.debug("stems cache hit: %s", output_dir)
        return cached

    # Lazy import — demucs pulls torch (~2 GB wheel) + loads model
    # weights into RAM. Callers that just want to *read* a cached stem
    # dir shouldn't pay any of that cost. ImportError surfaces as the
    # distinct ``StemsDependencyError`` so the RPC layer can return a
    # "pip install [stems]" hint cleanly.
    try:
        import demucs.api  # type: ignore[import-not-found]
    except ImportError as exc:
        raise StemsDependencyError(
            "stems feature not installed: pip install hypehouse-copilot[stems]"
        ) from exc

    log.info("running demucs (%s) on %s", model, audio_path)
    separator = demucs.api.Separator(model=model)
    # demucs returns a (origin, stems_dict) tuple where ``stems_dict``
    # maps stem name -> torch.Tensor (channels × samples).
    _origin, stems = separator.separate_audio_file(str(audio_path))

    written: dict[str, Path] = {}
    for name in STEM_NAMES:
        if name not in stems:
            raise RuntimeError(
                f"demucs did not produce stem '{name}' "
                f"(got: {sorted(stems.keys())}); model={model}"
            )
        out_path = output_dir / f"{name}.wav"
        # demucs.api.save_audio writes a 44.1 kHz stereo PCM_S16 WAV by
        # default — matches our cache shape, no extra args needed.
        demucs.api.save_audio(stems[name], str(out_path), samplerate=separator.samplerate)
        written[name] = out_path

    return written


def default_stems_root() -> Path:
    """User-level stem cache root.

    Default location is ``~/.local/share/hypehouse-live/stems/`` — XDG
    base-dir compliant on Linux, harmless on macOS / Windows (just a
    hidden dir under the user's home). Per-track caches go in
    ``<root>/<track_id>/``.
    """
    return Path("~/.local/share/hypehouse-live/stems").expanduser()
