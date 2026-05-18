// StemRack.tsx — 4-stem mute toggles per deck (vocals / drums / bass /
// other). Sits beneath the EQ knobs on each `Deck` panel.
//
// Behaviour:
//   * In `stem_mode = true` (deck loaded via `DeckLoadStems`), each
//     button toggles `Deck.stem_gains[i]` between 1.0 (audible) and
//     0.0 (muted) by emitting an externally-tagged `SetStemGain`
//     event.
//   * In `stem_mode = false` (full-mix playback), the buttons are
//     disabled with a tooltip ("Load stems first") so the user is
//     never confused by a no-op click.
//
// Wire shape lives in `engine/src/state.rs` —
// `EventKind::SetStemGain { deck, stem: u8, gain: f32 }`. Stem index
// ordering MUST match `STEM_ORDER` in `store/stems.ts`
// (`[vocals, drums, bass, other]`).

import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId } from "../store/engine";
import { STEM_ORDER, type StemName } from "../store/stems";

export interface StemRackProps {
  deck: DeckId;
  /** Mirror of `Deck::stem_gains` — `[vocals, drums, bass, other]`. */
  stemGains: readonly [number, number, number, number];
  /** True after `DeckLoadStems`; false in full-mix mode. */
  stemMode: boolean;
  client: JsonRpcWS;
}

/**
 * Single-letter labels per stem — vertical button uses the initial so
 * the rack stays compact. Order matches `STEM_ORDER`.
 */
const STEM_LABELS: Readonly<Record<StemName, string>> = {
  vocals: "V",
  drums: "D",
  bass: "B",
  other: "O",
};

/**
 * Per-stem accent colour. Pink / red / purple / cyan map to the
 * canonical "demucs four-stem palette" most DJ-software UIs adopt
 * (matches Serato / VirtualDJ's stem rack colouring so muscle memory
 * carries over).
 */
const STEM_COLOR: Readonly<Record<StemName, string>> = {
  vocals: "#ff5fa2",
  drums: "#ff4d4d",
  bass: "#a259ff",
  other: "#00d1c1",
};

const rackStyle: CSSProperties = {
  display: "flex",
  gap: 6,
  padding: "6px 0",
};

/**
 * Button style. Active (audible) shows the stem's accent colour as a
 * solid background; muted dims it to a 1.5px border on a near-black
 * fill so the contrast is glanceable from across the room.
 *
 * When the deck is in full-mix mode the rack collapses to a grayscale
 * look + the cursor flips to `not-allowed` — matches the EffectRack
 * convention for empty-slot affordance.
 */
const buttonStyle = (
  color: string,
  active: boolean,
  disabled: boolean,
): CSSProperties => ({
  flex: 1,
  height: 36,
  border: `1.5px solid ${disabled ? "#444" : color}`,
  borderRadius: 4,
  background: disabled
    ? "#222"
    : active
      ? color
      : "#0e0e0e",
  color: disabled ? "#666" : active ? "#0e0e0e" : color,
  cursor: disabled ? "not-allowed" : "pointer",
  fontFamily: "monospace",
  fontWeight: 700,
  fontSize: 14,
  letterSpacing: "0.05em",
  // Disabled tooltip needs a hint that the button is wired but inert.
  // `opacity` is intentionally NOT touched — it would clash with the
  // muted-vs-audible visual difference for the enabled state.
  transition: "background 80ms linear, color 80ms linear",
});

const submitSetStemGain = (
  client: JsonRpcWS,
  deck: DeckId,
  stem: number,
  gain: number,
): void => {
  // Externally-tagged enum — matches `EventKind::SetStemGain` serde
  // shape in `engine/src/state.rs`.
  void client
    .call("submit_event", { SetStemGain: { deck, stem, gain } })
    .catch((): void => undefined);
};

/**
 * Single-stem mute toggle. Pulled out so the rack itself stays a
 * trivial map over `STEM_ORDER` — easier to read at the call site +
 * each button gets its own stable test id (`stem-A-vocals` etc.).
 */
interface StemMuteButtonProps {
  deck: DeckId;
  index: number;
  name: StemName;
  gain: number;
  stemMode: boolean;
  client: JsonRpcWS;
}

const StemMuteButton = ({
  deck,
  index,
  name,
  gain,
  stemMode,
  client,
}: StemMuteButtonProps): JSX.Element => {
  // "Audible" iff gain > 0.01 — defends against float drift if the
  // engine ever sends 0.000001 from a future automation lane.
  const audible = gain > 0.01;
  const disabled = !stemMode;
  const color = STEM_COLOR[name];
  const onClick = (): void => {
    if (disabled) return;
    submitSetStemGain(client, deck, index, audible ? 0 : 1);
  };
  // ARIA toggle pattern — the button itself is the toggle handle so
  // `aria-pressed` carries the on/off state (a separate label would
  // double-announce).
  return (
    <button
      type="button"
      aria-label={`${name} ${audible ? "audible" : "muted"}`}
      aria-pressed={audible}
      data-testid={`stem-${deck}-${name}`}
      data-stem-color={color}
      disabled={disabled}
      title={
        disabled ? "Load stems first" : `${name} ${audible ? "audible" : "muted"}`
      }
      style={buttonStyle(color, audible, disabled)}
      onClick={onClick}
    >
      {STEM_LABELS[name]}
    </button>
  );
};

/**
 * 4-stem mute rack. Renders one button per entry in `STEM_ORDER`. The
 * rack itself has no internal state — every click round-trips through
 * the engine + comes back via `state_changed`, so a contested edit
 * (UI + MIDI controller both flipping) resolves to whichever the
 * reducer applied last.
 */
export const StemRack = ({
  deck,
  stemGains,
  stemMode,
  client,
}: StemRackProps): JSX.Element => {
  return (
    <div
      role="group"
      aria-label={`Stem mutes ${deck}`}
      data-testid={`stem-rack-${deck}`}
      style={rackStyle}
    >
      {STEM_ORDER.map((name: StemName, index: number): JSX.Element => (
        <StemMuteButton
          key={name}
          deck={deck}
          index={index}
          name={name}
          gain={stemGains[index] ?? 1}
          stemMode={stemMode}
          client={client}
        />
      ))}
    </div>
  );
};
