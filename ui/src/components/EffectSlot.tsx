// Single effect slot card. Renders inside <EffectRack/>.
//
// Layout (top → bottom):
//   1. Slot header  : "FX1/FX2/FX3" + effect dropdown (None / Filter /
//                     Echo / Reverb / Gate)
//   2. Wet/Dry knob + enabled toggle
//   3. Param controls (<EffectParamControl/> — Knob or button-group)
//
// All user input fires `submit_event` with the matching EventKind.
// The engine reducer is the source of truth — we read `effects[slot]`
// from props for display and never store local control state.

import type { CSSProperties, JSX, ChangeEvent } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId, EffectSlotState } from "../store/engine";
import type {
  EffectManifest,
  EffectManifestEntry,
} from "../store/effectsManifest";
import { Knob } from "./Knob";
import { Button } from "./Button";
import { EffectParamControl } from "./EffectParamControl";

export interface EffectSlotProps {
  deck: DeckId;
  slot: number; // 0..2
  state: EffectSlotState;
  manifest: EffectManifest;
  client: JsonRpcWS;
}

// Effect-id → tint for the active-slot border (ADR-006 visual feel).
// Empty slot keeps the muted grey baseline so the user can scan the
// rack and instantly see which slots are loaded.
const slotTint = (effectId: number): string => {
  switch (effectId) {
    case 1:
      return "#3a7ab0"; // filter - blue
    case 2:
      return "#b07a3a"; // echo - amber
    case 3:
      return "#7a3ab0"; // reverb - purple
    case 4:
      return "#b03a4a"; // gate - red
    default:
      return "#333";
  }
};

const cardStyle = (effectId: number): CSSProperties => ({
  border: `1px solid ${slotTint(effectId)}`,
  background: effectId === 0 ? "#161616" : "#1a1a1a",
  padding: 6,
  display: "flex",
  flexDirection: "column",
  gap: 6,
  fontFamily: "monospace",
  fontSize: 11,
  color: effectId === 0 ? "#777" : "#ddd",
  minWidth: 130,
});

const headerStyle: CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  alignItems: "center",
  gap: 4,
};

const rowStyle: CSSProperties = {
  display: "flex",
  gap: 6,
  alignItems: "flex-end",
  flexWrap: "wrap",
};

const selectStyle: CSSProperties = {
  background: "#1f1f1f",
  color: "#ddd",
  border: "1px solid #333",
  fontFamily: "monospace",
  fontSize: 11,
  padding: "2px 4px",
};

const enableBtnStyle: CSSProperties = { padding: "2px 6px", fontSize: 10 };

const submit = (
  client: JsonRpcWS,
  payload: Record<string, unknown>,
): void => {
  void client.call("submit_event", payload).catch((): void => undefined);
};

const findEffect = (
  manifest: EffectManifest,
  effectId: number,
): EffectManifestEntry | undefined =>
  manifest.find((e): boolean => e.id === effectId);

export const EffectSlot = ({
  deck,
  slot,
  state,
  manifest,
  client,
}: EffectSlotProps): JSX.Element => {
  const active = findEffect(manifest, state.effect_id);

  const onAssign = (ev: ChangeEvent<HTMLSelectElement>): void => {
    const nextId = Number(ev.target.value);
    if (nextId === 0) {
      submit(client, { EffectClear: { deck, slot } });
    } else {
      submit(client, { EffectAssign: { deck, slot, effect_id: nextId } });
    }
  };
  const onWetDry = (next: number): void =>
    submit(client, { EffectWetDry: { deck, slot, value: next } });
  const onEnable = (): void =>
    submit(client, {
      EffectEnable: { deck, slot, enabled: !state.enabled },
    });

  return (
    <div
      style={cardStyle(state.effect_id)}
      data-testid={`fx-slot-${deck}-${slot}`}
      aria-label={`effect-slot-${deck}-${slot}`}
    >
      <div style={headerStyle}>
        <strong>FX{slot + 1}</strong>
        <select
          style={selectStyle}
          value={state.effect_id}
          onChange={onAssign}
          aria-label={`effect-select-${deck}-${slot}`}
          data-testid={`fx-select-${deck}-${slot}`}
        >
          <option value={0}>None</option>
          {manifest.map(
            (e): JSX.Element => (
              <option key={e.id} value={e.id}>
                {e.name}
              </option>
            ),
          )}
        </select>
      </div>

      <div style={rowStyle}>
        <Knob
          label="W/D"
          min={0}
          max={1}
          step={0.01}
          value={state.wet_dry}
          onChange={onWetDry}
          resetValue={0.5}
          vertical={false}
          size={60}
          testId={`fx-wetdry-${deck}-${slot}`}
          ariaLabel={`wet-dry-deck-${deck}-slot-${slot}`}
        />
        <Button
          onClick={onEnable}
          pressed={state.enabled}
          disabled={state.effect_id === 0}
          testId={`fx-enable-${deck}-${slot}`}
          ariaLabel={`enable-deck-${deck}-slot-${slot}`}
          style={enableBtnStyle}
        >
          {state.enabled ? "ON" : "OFF"}
        </Button>
      </div>

      {active ? (
        <div style={rowStyle} data-testid={`fx-params-${deck}-${slot}`}>
          {active.params.map(
            (p): JSX.Element => (
              <EffectParamControl
                key={p.name}
                deck={deck}
                slot={slot}
                state={state}
                param={p}
                client={client}
              />
            ),
          )}
        </div>
      ) : null}
    </div>
  );
};
