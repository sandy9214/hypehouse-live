// One control row for a single effect parameter. Renders either a
// continuous Knob (typical case — cutoff_hz, resonance, time_ms, …)
// or a discrete button-group (e.g. Filter `mode` 0=LP / 1=HP / 2=BP)
// chosen heuristically from the descriptor's integer range.
//
// Extracted from EffectSlot.tsx to keep that file under the 250-line
// component budget. Pure presentation — relies on the parent to thread
// the JSON-RPC submit callback.

import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId, EffectSlotState } from "../store/engine";
import type { EffectParamDescriptor } from "../store/effectsManifest";
import { Knob } from "./Knob";
import { Button } from "./Button";

const FILTER_MODE_LABELS: ReadonlyArray<string> = ["LP", "HP", "BP"];
const modeBtnStyle: CSSProperties = { padding: "2px 6px", fontSize: 10 };

export const stepFor = (p: EffectParamDescriptor): number => {
  const range = p.max - p.min;
  if (range <= 0) return 0.01;
  // Integer-keyed small ranges get integer step; everything else
  // gets 1/100 of the range for smooth dragging.
  if (Number.isInteger(p.min) && Number.isInteger(p.max) && range <= 16) {
    return 1;
  }
  return Math.max(range / 100, 0.001);
};

export const isDiscreteEnumParam = (p: EffectParamDescriptor): boolean =>
  Number.isInteger(p.min) &&
  Number.isInteger(p.max) &&
  p.max - p.min <= 4 &&
  p.max - p.min >= 1;

const enumLabel = (paramName: string, value: number): string => {
  if (paramName === "mode") {
    return FILTER_MODE_LABELS[value] ?? value.toString();
  }
  return value.toString();
};

const paramValue = (
  state: EffectSlotState,
  p: EffectParamDescriptor,
): number => state.params[p.name] ?? p.default;

const submitParam = (
  client: JsonRpcWS,
  deck: DeckId,
  slot: number,
  paramName: string,
  value: number,
): void => {
  void client
    .call("submit_event", {
      EffectParam: { deck, slot, param: paramName, value },
    })
    .catch((): void => undefined);
};

export interface EffectParamControlProps {
  deck: DeckId;
  slot: number;
  state: EffectSlotState;
  param: EffectParamDescriptor;
  client: JsonRpcWS;
}

export const EffectParamControl = ({
  deck,
  slot,
  state,
  param,
  client,
}: EffectParamControlProps): JSX.Element => {
  const value = paramValue(state, param);
  const onParam = (next: number): void =>
    submitParam(client, deck, slot, param.name, next);

  if (isDiscreteEnumParam(param)) {
    const optionCount = Math.round(param.max - param.min) + 1;
    const options = Array.from(
      { length: optionCount },
      (_, i): number => param.min + i,
    );
    return (
      <div data-testid={`fx-${deck}-${slot}-param-${param.name}`}>
        <span aria-hidden="true">{param.name.toUpperCase()}</span>
        <div style={{ display: "flex", gap: 2, marginTop: 2 }}>
          {options.map(
            (opt): JSX.Element => (
              <Button
                key={opt}
                onClick={(): void => onParam(opt)}
                pressed={Math.round(value) === opt}
                testId={`fx-${deck}-${slot}-${param.name}-${opt}`}
                ariaLabel={`${param.name}-${opt}-deck-${deck}-slot-${slot}`}
                style={modeBtnStyle}
              >
                {enumLabel(param.name, opt)}
              </Button>
            ),
          )}
        </div>
      </div>
    );
  }

  return (
    <Knob
      label={param.name.toUpperCase()}
      min={param.min}
      max={param.max}
      step={stepFor(param)}
      value={value}
      onChange={onParam}
      resetValue={param.default}
      vertical={false}
      size={80}
      testId={`fx-${deck}-${slot}-param-${param.name}`}
      ariaLabel={`${param.name}-deck-${deck}-slot-${slot}`}
    />
  );
};
