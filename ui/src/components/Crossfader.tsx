// Crossfader — slider input wired to `submit_event { Crossfader }`.
//
// ADR-002: master section owns the crossfader. We optimistically read
// from local store but submit through the engine so the event log
// stays the single source of truth.
//
// Wire shape mirrors the engine's externally-tagged serde `EventKind`
// enum (see `engine/src/state.rs::EventKind::Crossfader { value }`):
//   { "Crossfader": { "value": 0.5 } }
// — same form used by `midi/translator.ts` + `keyboardListener.ts`.

import type { ChangeEvent } from "react";
import type { JsonRpcWS } from "../ws/client";

export interface CrossfaderProps {
  client: JsonRpcWS;
  value: number; // 0..1
}

export const Crossfader = ({ client, value }: CrossfaderProps): JSX.Element => {
  const handleChange = (ev: ChangeEvent<HTMLInputElement>): void => {
    const next = Number(ev.target.value) / 1000;
    // Fire-and-forget; future PR adds error toast.
    void client.call("submit_event", { Crossfader: { value: next } });
  };

  return (
    <div
      style={{
        padding: 12,
        background: "#0e0e0e",
        color: "#ddd",
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        gap: 4,
      }}
    >
      <label
        htmlFor="crossfader"
        style={{ fontFamily: "monospace", fontSize: 12 }}
      >
        CROSSFADER · {value.toFixed(2)}
      </label>
      <input
        id="crossfader"
        type="range"
        min={0}
        max={1000}
        step={1}
        value={Math.round(value * 1000)}
        onChange={handleChange}
        style={{ width: "60%" }}
        data-testid="crossfader-input"
        aria-label="crossfader"
      />
    </div>
  );
};
