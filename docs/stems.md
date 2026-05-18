# Stem separation — vocals / drums / bass / other

**Status**: v0.1 scaffold — analyzer + library + RPC only. UI controls
and engine-side stem-deck playback are tracked in follow-up PRs.

**Owner**: co-pilot service (`copilot/stems.py`,
`copilot/library.py`, `copilot/library_rpc.py`).

## What this is

Stem-aware mashing wants to mix the **vocals** of one track over the
**drums** of another. To do that the engine needs per-track stem WAVs
on disk so it can load them as independent decks.

This PR adds the scaffold that produces those WAVs. It runs Facebook's
[demucs](https://github.com/facebookresearch/demucs) `htdemucs` model
on a library track and writes four files into a per-track cache
directory.

```
<output_dir>/
  vocals.wav   ← isolated vocal stem
  drums.wav    ← isolated drum stem
  bass.wav     ← isolated bass-line stem
  other.wav    ← everything else (synths, guitar, FX, …)
```

Each is stereo 44.1 kHz / 16-bit PCM (demucs's default writer output).

## Enabling the feature

Demucs is heavy:

* The PyTorch wheel weighs ~2 GB.
* The `htdemucs` model checkpoint is another ~2 GB and downloads on
  first invocation into `~/.cache/torch/hub/checkpoints/`.

We therefore keep demucs an **optional** dependency. To opt in:

```bash
pip install -e ".[stems]"      # editable install in the copilot dir
# or
pip install "hypehouse-copilot[stems]"
```

Without the opt-in, every `library.compute_stems` JSON-RPC call returns
`-32000` with `message: "stems feature not installed: pip install
hypehouse-copilot[stems]"`. The co-pilot service still starts and every
other `library.*` method continues to work — feature absence never
takes the service down.

## Performance

Measured against a 3-minute pop track on `htdemucs` (4-stem default):

| Hardware                | Wall-clock |
|-------------------------|------------|
| CUDA (RTX 3090)         | ~30 s      |
| Apple Silicon (Metal)   | ~30 s      |
| CPU only (16-core x86)  | ~3 min     |

Treat this as an **offline ingest** path. Stem separation is far too
slow for any real-time codepath; the engine consumes pre-rendered
WAVs from the cache.

## On-disk size

A 3-minute stereo 44.1 kHz / 16-bit WAV is ~31 MB. Four stems per
track ⇒ **~125 MB per track**. For a 200-track library budget ~25 GB
of cache disk. Operators sizing the host for stems should plan
accordingly.

The default cache root is `~/.local/share/hypehouse-live/stems/` (XDG
base-dir compliant on Linux, a harmless dotted directory on macOS /
Windows). Each track gets its own subdirectory keyed by `track_id`.

## Library API

Python callers go through `TrackLibrary`:

```python
from copilot.library import TrackLibrary

lib = TrackLibrary("~/.hypehouse-live/library.db")
stems = lib.compute_track_stems("kanye-stronger")
# {"vocals": Path(".../kanye-stronger/vocals.wav"),
#  "drums":  Path(".../kanye-stronger/drums.wav"),
#  "bass":   Path(".../kanye-stronger/bass.wav"),
#  "other":  Path(".../kanye-stronger/other.wav")}

status, stems_dir = lib.get_stems_status("kanye-stronger")
# ("ready", ".../kanye-stronger")
```

The DB row's `stems_status` column transitions
`null → pending → ready` on success, or `null → pending → failed` if
demucs raises. Failure detail is logged but not persisted (a future
`stems_status_detail` column is the obvious extension).

## JSON-RPC API

Two new methods on the `library.*` namespace — see
[`docs/api/ws-protocol.md`](api/ws-protocol.md) for the full wire
schemas:

* `library.compute_stems` — kicks off stem separation as a background
  task, returns `{status: "pending"}` immediately. Re-calling while a
  task is in flight is a no-op.
* `library.get_stems` — polls the current state. Returns
  `{status, stems}` where `stems` is the four-WAV path map when
  `status == "ready"`, else `null`.

## Caching

`compute_stems` short-circuits when all four output WAVs already
exist and are non-empty. Re-running stem separation is the expensive
part; a sub-millisecond stat check is the right tradeoff. Zero-byte
WAVs (from a killed previous run) are treated as missing so a
half-written cache doesn't masquerade as a hit.

## Not in scope (deferred)

* UI controls for triggering stem separation from the library panel.
* Engine-side integration — loading stems into individual decks
  rather than the full mixed track.
* Stem-aware mashability scoring (e.g. vocal-only key compatibility).
* GPU vs CPU device selection knob (currently demucs auto-picks).
* Multi-model selection (only `htdemucs` is exposed today).
