"""Key-matching helpers — Camelot wheel → semitone offset.

The "key match" feature pitch-shifts deck B's audio so its tonal centre
lines up with deck A's. The Camelot wheel encodes both major (B suffix)
and minor (A suffix) keys on a 12-step circle of fifths, so moving N
clockwise positions corresponds to N perfect-fifths up — i.e. ``N * 7``
semitones — modulo the octave.

Two facts the rest of the module hangs off of:

1. **Number step = 7 semitones.** ``8B`` (C major) → ``9B`` (G major) is a
   perfect 5th, which is 7 semitones. Going the long way around the wheel
   (12 steps) returns to the same pitch class, modulo octaves.
2. **Letter swap (A↔B, same number) = 0 semitones.** ``8B`` (C major) and
   ``8A`` (A minor) share the same set of notes — they're relative
   minor/major. Pitching the audio doesn't change the key class; only the
   tonic perception does. Returning 0 here is deliberate: a host who
   labelled their track as the relative minor doesn't want a phantom
   transposition applied just because the Camelot letter flipped.

The returned offset is wrapped into ``[-6, 6]`` semitones (a tritone in
either direction). Anything wider would mean stretching audio so far it
crosses an octave boundary — and the shorter path is always perceptually
equivalent for tonal alignment.

Function returns ``float`` (not ``int``) to leave room for a future
quarter-tone refinement and to match the wire type on
``EventKind::PitchBend { semitones: f32 }``.
"""
from __future__ import annotations

# Each Camelot number step around the wheel = one perfect 5th = 7 semitones.
# Going N steps clockwise = N * 7 semitones up (mod 12).
SEMITONES_PER_CAMELOT_STEP = 7

# Total semitones in an octave — used to fold the raw N*7 value back into
# the [-6, 6] tritone window.
SEMITONES_PER_OCTAVE = 12


def _parse_camelot(code: str) -> tuple[int, str] | None:
    """Split ``"8B"`` → ``(8, "B")``. Returns ``None`` on malformed input.

    Mirrors the leniency of :func:`copilot.library.camelot_distance`:
    strips whitespace, upper-cases, accepts ``A`` / ``B`` letter,
    rejects everything else (returns ``None`` so the public API can
    short-circuit to ``0.0`` rather than raise — a missing key on a
    track simply disables the match button UI-side).
    """
    if not isinstance(code, str):
        return None
    s = code.strip().upper()
    if len(s) < 2:
        return None
    letter = s[-1]
    if letter not in ("A", "B"):
        return None
    try:
        number = int(s[:-1])
    except ValueError:
        return None
    if not (1 <= number <= 12):
        return None
    return number, letter


def camelot_to_semitones(from_key: str, to_key: str) -> float:
    """Semitone offset to transpose ``from_key`` audio into ``to_key``.

    Returns a float in ``[-6.0, 6.0]``. Positive = pitch up. ``0.0`` is
    returned for:

    * identical keys (e.g. ``"8B" → "8B"``),
    * relative minor/major pairs (``"8B" ↔ "8A"`` — same notes,
      different tonic perception; audio doesn't move),
    * missing / malformed inputs (caller should disable the UI button
      based on the underlying ``camelot_key`` field rather than infer
      that ``0.0`` means "no shift needed").

    The wrap rule: raw offset = ``(to_num - from_num) * 7``; this lives
    on a 12-step ring, so any value outside ``(-6, 6]`` is rotated to
    its shorter-path equivalent. ``8B → 1B`` (5 steps clockwise = +35
    semitones) wraps to ``-1`` semitone — the same pitch class one
    semitone down is the perceptually-shorter path.
    """
    a = _parse_camelot(from_key)
    b = _parse_camelot(to_key)
    if a is None or b is None:
        return 0.0
    from_num, from_letter = a
    to_num, to_letter = b

    # Relative minor/major (same number, different letter) shares the
    # same notes — no pitch transposition. Two tracks marked 8B vs 8A
    # are already harmonically aligned in the wheel sense; the host
    # picked the labelling that matched their root-note preference.
    if from_num == to_num and from_letter != to_letter:
        return 0.0

    # Raw circle-of-fifths distance in semitones. We treat A and B
    # rings as a single ring for transposition: a host pitching the
    # audio to bring 8A → 9B should still hear a perfect-5th rise,
    # because the audio's pitch class shifts independently of the
    # major/minor labelling. The 0-return above handles the only
    # "letter swap with no pitch change" special case.
    raw = (to_num - from_num) * SEMITONES_PER_CAMELOT_STEP

    # Fold into (-6, 6]. Standard mod-12 then re-centre around 0.
    folded = raw % SEMITONES_PER_OCTAVE
    if folded > SEMITONES_PER_OCTAVE // 2:
        folded -= SEMITONES_PER_OCTAVE
    return float(folded)
