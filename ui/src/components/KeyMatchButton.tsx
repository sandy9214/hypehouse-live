// KeyMatchButton — one-click harmonic pitch alignment between two decks.
//
// Click the button on Deck B, and the co-pilot's `key_match.compute_offset`
// RPC computes the semitone delta required to pitch B's audio into A's
// key. The result is emitted as a `PitchBend` event so the engine
// applies it via the existing pitch-shifter (no new audio plumbing).
//
// State machine:
//   * disabled  — either deck has no library row OR no parseable key.
//   * "Match"   — ready to fire (default idle).
//   * "…"       — RPC in flight (button briefly disabled).
//   * "Matched" — last shift successful; clears on next track load.
//   * "Match"   — error path (button re-arms; we swallow the toast for v0.1).
//
// Wire surface:
//   * `key_match.compute_offset {from_track_id, to_track_id}`
//     → `{semitones: number}`  (-6..+6, parallel transposition).
//   * `submit_event { PitchBend: { deck, semitones } }` — same shape
//     the existing Pitch knob uses (engine/src/state.rs:233).

import { useEffect, useState, type CSSProperties, type JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId } from "../store/engine";

export interface KeyMatchButtonProps {
  /** Deck this button operates on (the one that will be pitch-shifted). */
  readonly deck: DeckId;
  /** Camelot key (`"8B"`, `"10A"`) of *this* deck's loaded track. `null` when no track loaded. */
  readonly thisKey: string | null;
  /** Library track id of *this* deck's loaded track. `null` disables the button. */
  readonly thisTrackId: string | null;
  /** Camelot key of the OTHER (reference) deck's track. `null` disables. */
  readonly otherKey: string | null;
  /** Library track id of the OTHER deck's track. `null` disables. */
  readonly otherTrackId: string | null;
  readonly client: JsonRpcWS;
}

interface ComputeOffsetResult {
  readonly semitones: number;
}

/** Quick local sanity check — a parseable Camelot code is `<1..12><A|B>`. */
const isParseable = (k: string | null): boolean => {
  if (k === null) return false;
  if (k.length < 2 || k.length > 3) return false;
  const letter = k[k.length - 1].toUpperCase();
  if (letter !== "A" && letter !== "B") return false;
  const num = Number(k.slice(0, -1));
  return Number.isInteger(num) && num >= 1 && num <= 12;
};

const baseStyle: CSSProperties = {
  background: "#222",
  color: "#ddd",
  border: "1px solid #444",
  borderRadius: 4,
  padding: "2px 8px",
  fontFamily: "monospace",
  fontSize: 12,
};

const disabledStyle: CSSProperties = {
  ...baseStyle,
  opacity: 0.45,
  cursor: "not-allowed",
};

const matchedStyle: CSSProperties = {
  ...baseStyle,
  background: "#1f3a2a",
  borderColor: "#3f7a55",
  color: "#9ad6b3",
};

export const KeyMatchButton = ({
  deck,
  thisKey,
  thisTrackId,
  otherKey,
  otherTrackId,
  client,
}: KeyMatchButtonProps): JSX.Element => {
  type Status = "idle" | "pending" | "matched";
  const [status, setStatus] = useState<Status>("idle");

  // Clear the "Matched" badge whenever either deck's loaded track id
  // changes — a fresh load wipes the previously-applied bend (engine
  // clamps pitch_semitones back to 0 on DeckLoad), so the visual
  // indicator should reset too.
  useEffect((): void => {
    setStatus("idle");
  }, [thisTrackId, otherTrackId]);

  const haveBothKeys =
    thisTrackId !== null &&
    otherTrackId !== null &&
    isParseable(thisKey) &&
    isParseable(otherKey);

  const onClick = async (): Promise<void> => {
    if (!haveBothKeys || thisTrackId === null || otherTrackId === null) {
      return;
    }
    setStatus("pending");
    try {
      // Other deck = "to" (reference key). This deck = "from" (gets
      // shifted into the reference). compute_offset returns the
      // semitones needed to bring `from_track_id` UP into
      // `to_track_id`'s key — exactly what PitchBend expects.
      const result = await client.call<ComputeOffsetResult>(
        "key_match.compute_offset",
        { from_track_id: thisTrackId, to_track_id: otherTrackId },
      );
      const semitones = Number(result?.semitones ?? 0);
      // Fire the existing PitchBend event — engine reducer at
      // engine/src/state.rs:814 clamps to its own pitch range.
      await client.call("submit_event", {
        PitchBend: { deck, semitones },
      });
      setStatus("matched");
    } catch {
      // Swallow — v0.1 has no toast layer (matches the rest of
      // Deck.tsx's fire-and-forget pattern). Re-arm so the operator
      // can retry.
      setStatus("idle");
    }
  };

  const label =
    status === "pending" ? "Key…" : status === "matched" ? "Matched" : "Key →";

  const style = !haveBothKeys
    ? disabledStyle
    : status === "matched"
      ? matchedStyle
      : baseStyle;

  return (
    <button
      type="button"
      aria-label={`Match key on deck ${deck}`}
      data-testid={`key-match-${deck}`}
      data-state={status}
      disabled={!haveBothKeys || status === "pending"}
      onClick={(): void => {
        void onClick();
      }}
      style={style}
    >
      {label}
    </button>
  );
};
