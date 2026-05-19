// Sidechain compressor settings panel (issue #119).
//
// Renders a toggle + trigger-deck switch + 5 param knobs (threshold,
// ratio, attack, release, makeup). Reads current values from the
// engine state mirror; every input fires `submit_event` with the
// matching event variant (SetSidechainEnabled or SetSidechainParams).
//
// The audio DSP that actually ducks the non-trigger deck is deferred
// to a follow-up PR — for now this panel drives only the engine's
// `state::SidechainConfig` mirror (so the wire surface + persistence
// flow are testable end-to-end before any audio path lands).

import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId, SidechainConfig } from "../store/engine";
import { DEFAULT_SIDECHAIN } from "../store/engine";
import { Knob } from "./Knob";
import { Button } from "./Button";

export interface SidechainPanelProps {
  readonly client: JsonRpcWS;
  readonly state?: SidechainConfig | null;
  /**
   * Live gain reduction (dB) from the engine's audio-thread side-channel
   * atomic, exposed via `engine.state_changed.sidechain_gain_reduction_db`.
   * `<= 0` while ducking; `0` when bypassed. Drives the vertical meter.
   */
  readonly grDb?: number;
}

const containerStyle: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: "0.4rem",
  padding: "0.6rem",
  border: "1px solid #2a2a2a",
  borderRadius: "0.4rem",
  background: "#111",
  color: "#ddd",
  fontFamily: "system-ui, sans-serif",
  fontSize: "0.85rem",
  maxWidth: "420px",
};

const labelStyle: CSSProperties = {
  fontWeight: 600,
  color: "#aaa",
};

const headerRowStyle: CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: "0.6rem",
};

const knobRowStyle: CSSProperties = {
  display: "flex",
  gap: "0.5rem",
  flexWrap: "wrap",
  alignItems: "flex-end",
};

const triggerBtnStyle: CSSProperties = {
  padding: "2px 8px",
  fontSize: 11,
};

/** Visual ceiling for the GR meter. Compressor reduction past -24 dB
 *  is far outside normal DJ ducking depth — clamp the bar so the
 *  common 0..-12 dB range fills the meter usefully. */
const METER_MIN_DB = -24;
const METER_HEIGHT = 60;

const meterContainerStyle: CSSProperties = {
  position: "relative",
  width: 14,
  height: METER_HEIGHT,
  background: "#181818",
  border: "1px solid #2a2a2a",
  borderRadius: 2,
  overflow: "hidden",
  alignSelf: "flex-end",
};

const meterScaleStyle: CSSProperties = {
  fontSize: 9,
  color: "#666",
  display: "flex",
  flexDirection: "column",
  justifyContent: "space-between",
  height: METER_HEIGHT,
  marginLeft: 4,
  alignSelf: "flex-end",
};

const meterLabelStyle: CSSProperties = {
  fontSize: 9,
  color: "#888",
  letterSpacing: 0.5,
  marginRight: 4,
  alignSelf: "flex-end",
};

const submit = (
  client: JsonRpcWS,
  payload: Record<string, unknown>,
): void => {
  void client.call("submit_event", payload).catch((): void => undefined);
};

export const SidechainPanel = ({
  client,
  state,
  grDb,
}: SidechainPanelProps): JSX.Element => {
  const cfg = state ?? DEFAULT_SIDECHAIN;
  // Clamp GR into [METER_MIN_DB, 0] then map to a 0..1 bar fraction.
  // `grDb` undefined / NaN → empty meter.
  const grClamped =
    typeof grDb === "number" && Number.isFinite(grDb)
      ? Math.max(METER_MIN_DB, Math.min(0, grDb))
      : 0;
  const meterFraction = Math.min(1, -grClamped / -METER_MIN_DB);
  const meterFillPx = Math.round(meterFraction * METER_HEIGHT);

  const onToggle = (): void =>
    submit(client, {
      SetSidechainEnabled: { enabled: !cfg.enabled },
    });

  const setTrigger = (deck: DeckId): void =>
    submit(client, {
      SetSidechainParams: {
        trigger_deck: deck,
        threshold_db: null,
        ratio: null,
        attack_ms: null,
        release_ms: null,
        makeup_gain_db: null,
      },
    });

  const updateParam = (
    field:
      | "threshold_db"
      | "ratio"
      | "attack_ms"
      | "release_ms"
      | "makeup_gain_db",
    value: number,
  ): void =>
    submit(client, {
      SetSidechainParams: {
        trigger_deck: null,
        threshold_db: field === "threshold_db" ? value : null,
        ratio: field === "ratio" ? value : null,
        attack_ms: field === "attack_ms" ? value : null,
        release_ms: field === "release_ms" ? value : null,
        makeup_gain_db: field === "makeup_gain_db" ? value : null,
      },
    });

  return (
    <div style={containerStyle} data-testid="sidechain-panel">
      <div style={headerRowStyle}>
        <span style={labelStyle}>Sidechain</span>
        <Button
          onClick={onToggle}
          pressed={cfg.enabled}
          testId="sidechain-toggle"
          ariaLabel="Sidechain compressor toggle"
          style={triggerBtnStyle}
        >
          {cfg.enabled ? "ON" : "OFF"}
        </Button>
        <span style={labelStyle}>Trigger</span>
        <Button
          onClick={(): void => setTrigger("A")}
          pressed={cfg.trigger_deck === "A"}
          testId="sidechain-trigger-A"
          ariaLabel="Set sidechain trigger to deck A"
          style={triggerBtnStyle}
        >
          A
        </Button>
        <Button
          onClick={(): void => setTrigger("B")}
          pressed={cfg.trigger_deck === "B"}
          testId="sidechain-trigger-B"
          ariaLabel="Set sidechain trigger to deck B"
          style={triggerBtnStyle}
        >
          B
        </Button>
      </div>

      <div style={knobRowStyle}>
        <span style={meterLabelStyle}>GR</span>
        <div
          style={meterContainerStyle}
          data-testid="sidechain-gr-meter"
          aria-label="Sidechain gain reduction"
          title={`${grClamped.toFixed(1)} dB`}
        >
          <div
            data-testid="sidechain-gr-meter-fill"
            style={{
              position: "absolute",
              left: 0,
              right: 0,
              top: 0,
              height: meterFillPx,
              background: "linear-gradient(180deg, #e0a800, #b34700)",
            }}
          />
        </div>
        <div style={meterScaleStyle}>
          <span>0</span>
          <span>-12</span>
          <span>-24</span>
        </div>
        <Knob
          label="Thr"
          min={-60}
          max={0}
          step={0.5}
          value={cfg.threshold_db}
          onChange={(v): void => updateParam("threshold_db", v)}
          resetValue={-12}
          size={54}
          testId="sidechain-threshold"
          ariaLabel="sidechain-threshold-db"
          vertical={false}
        />
        <Knob
          label="Ratio"
          min={1}
          max={20}
          step={0.5}
          value={cfg.ratio}
          onChange={(v): void => updateParam("ratio", v)}
          resetValue={4}
          size={54}
          testId="sidechain-ratio"
          ariaLabel="sidechain-ratio"
          vertical={false}
        />
        <Knob
          label="Att"
          min={0.1}
          max={100}
          step={0.5}
          value={cfg.attack_ms}
          onChange={(v): void => updateParam("attack_ms", v)}
          resetValue={5}
          size={54}
          testId="sidechain-attack"
          ariaLabel="sidechain-attack-ms"
          vertical={false}
        />
        <Knob
          label="Rel"
          min={10}
          max={2000}
          step={5}
          value={cfg.release_ms}
          onChange={(v): void => updateParam("release_ms", v)}
          resetValue={200}
          size={54}
          testId="sidechain-release"
          ariaLabel="sidechain-release-ms"
          vertical={false}
        />
        <Knob
          label="Mkp"
          min={0}
          max={24}
          step={0.5}
          value={cfg.makeup_gain_db}
          onChange={(v): void => updateParam("makeup_gain_db", v)}
          resetValue={0}
          size={54}
          testId="sidechain-makeup"
          ariaLabel="sidechain-makeup-gain-db"
          vertical={false}
        />
      </div>
    </div>
  );
};
