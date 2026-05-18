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
//
// Curve dropdown (engine-crossfader-curves PR): exposes the engine's
// `CrossfaderCurve` enum to the user. Wire shape mirrors the new
// `SetCrossfaderCurve` event:
//   { "SetCrossfaderCurve": { "curve": "Dipped" } }
// Variant names ("Linear" | "Dipped" | "Sharp" | "Scratch") are the
// load-bearing wire labels — serde external-tag default emits the
// bare PascalCase variant name.

import type { ChangeEvent } from "react";
import type { JsonRpcWS } from "../ws/client";

/**
 * Crossfader response curve — string union mirroring the Rust
 * `CrossfaderCurve` enum (engine/src/state.rs). Variant names are the
 * load-bearing wire labels (serde external-tag → bare variant name).
 */
export type CrossfaderCurve = "Linear" | "Dipped" | "Sharp" | "Scratch";

export const CROSSFADER_CURVES: readonly CrossfaderCurve[] = [
  "Linear",
  "Dipped",
  "Sharp",
  "Scratch",
];

export interface CrossfaderProps {
  client: JsonRpcWS;
  value: number; // 0..1
  /**
   * Active curve, mirrored from `engine.state_changed`. Defaults to
   * `"Linear"` when the engine snapshot doesn't include the field —
   * matches the engine's `#[serde(default)]` on the same field.
   */
  curve?: CrossfaderCurve;
}

/**
 * SVG icon previewing the curve shape. Pure presentational; same
 * `24x16` viewbox for all variants so the dropdown lines up. Each path
 * encodes the qualitative shape of `gain_b` across `x ∈ [0, 1]`.
 */
const CurveIcon = ({ curve }: { curve: CrossfaderCurve }): JSX.Element => {
  const stroke = "#9af";
  let path: string;
  switch (curve) {
    case "Linear":
      // Diagonal straight line bottom-left to top-right.
      path = "M2 14 L22 2";
      break;
    case "Dipped":
      // Smooth U-shape — energy dips in the middle (each side -3 dB).
      path = "M2 2 Q12 16 22 2";
      break;
    case "Sharp":
      // Step: flat low, vertical rise, flat high.
      path = "M2 14 L11 14 L11 2 L13 2 L13 14 L22 14";
      break;
    case "Scratch":
      // Cliff: long flat low, near-vertical jump at the edge, flat high.
      path = "M2 14 L20 14 L21 2 L22 2";
      break;
  }
  return (
    <svg
      width={24}
      height={16}
      viewBox="0 0 24 16"
      aria-hidden="true"
      data-testid={`crossfader-curve-icon-${curve.toLowerCase()}`}
    >
      <path
        d={path}
        fill="none"
        stroke={stroke}
        strokeWidth={1.5}
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
};

export const Crossfader = ({
  client,
  value,
  curve = "Linear",
}: CrossfaderProps): JSX.Element => {
  const handleChange = (ev: ChangeEvent<HTMLInputElement>): void => {
    const next = Number(ev.target.value) / 1000;
    // Fire-and-forget; future PR adds error toast.
    void client.call("submit_event", { Crossfader: { value: next } });
  };

  const handleCurveChange = (ev: ChangeEvent<HTMLSelectElement>): void => {
    const nextCurve = ev.target.value as CrossfaderCurve;
    // Defend against a stray DOM mutation injecting a non-enum value.
    if (!CROSSFADER_CURVES.includes(nextCurve)) return;
    void client.call("submit_event", {
      SetCrossfaderCurve: { curve: nextCurve },
    });
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
      <div
        style={{
          display: "flex",
          flexDirection: "row",
          alignItems: "center",
          gap: 8,
          fontFamily: "monospace",
          fontSize: 11,
        }}
      >
        <label htmlFor="crossfader-curve">CURVE</label>
        <CurveIcon curve={curve} />
        <select
          id="crossfader-curve"
          value={curve}
          onChange={handleCurveChange}
          data-testid="crossfader-curve-select"
          aria-label="crossfader curve"
          style={{
            background: "#1a1a1a",
            color: "#ddd",
            border: "1px solid #333",
            fontFamily: "monospace",
            fontSize: 11,
            padding: "2px 4px",
          }}
        >
          {CROSSFADER_CURVES.map((c): JSX.Element => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
      </div>
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
