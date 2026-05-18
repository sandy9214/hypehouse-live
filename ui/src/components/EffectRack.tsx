// Per-deck effects rack — renders 3 <EffectSlot/>s side by side.
//
// State.decks[deck].effects[3] feeds the slot states directly; the
// manifest comes from `useEffectsManifest()` in the parent. We keep
// this component dumb (no fetching, no RPC) so it stays trivial to
// snapshot-test and to compose with other layouts.

import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId, EffectSlotState } from "../store/engine";
import type { EffectManifest } from "../store/effectsManifest";
import { EffectSlot } from "./EffectSlot";

export interface EffectRackProps {
  deck: DeckId;
  effects: readonly [EffectSlotState, EffectSlotState, EffectSlotState];
  manifest: EffectManifest;
  client: JsonRpcWS;
}

const wrapStyle: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: 4,
};

const headerStyle: CSSProperties = {
  fontFamily: "monospace",
  fontSize: 11,
  color: "#888",
  letterSpacing: 0.5,
};

const rowStyle: CSSProperties = {
  display: "flex",
  gap: 4,
  alignItems: "stretch",
};

export const EffectRack = ({
  deck,
  effects,
  manifest,
  client,
}: EffectRackProps): JSX.Element => (
  <div
    style={wrapStyle}
    data-testid={`fx-rack-${deck}`}
    aria-label={`effects-rack-${deck}`}
  >
    <span style={headerStyle}>EFFECTS</span>
    <div style={rowStyle}>
      {effects.map(
        (slot, i): JSX.Element => (
          <EffectSlot
            key={i}
            deck={deck}
            slot={i}
            state={slot}
            manifest={manifest}
            client={client}
          />
        ),
      )}
    </div>
  </div>
);
