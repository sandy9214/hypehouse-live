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

import { useEffect, useRef, useState } from "react";
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

const oneShotRowStyle: CSSProperties = {
  display: "flex",
  gap: 3,
  alignItems: "center",
  flexWrap: "wrap",
};

const oneShotLabelStyle: CSSProperties = {
  fontSize: 9,
  color: "#888",
  marginRight: 2,
  letterSpacing: 0.5,
};

const oneShotBtnStyle: CSSProperties = {
  padding: "2px 5px",
  fontSize: 10,
  minWidth: 22,
};

const countdownStyle: CSSProperties = {
  fontSize: 9,
  color: "#e0a800",
  fontVariantNumeric: "tabular-nums",
  marginLeft: 4,
};

/** Industry-standard preset durations — match the loop bar presets row. */
const ONE_SHOT_BEAT_PRESETS: ReadonlyArray<number> = [1, 4, 8, 16];

/**
 * Anchored monotonic clock — pairs `performance.now()` (monotonic,
 * sub-ms) with a `Date.now()` epoch so the rAF tick stays smooth +
 * jump-free while the displayed value is still in the same scale as
 * the engine's wall-clock `ts_micros`. See issue #130.
 */
interface ClockAnchor {
  perfMs: number;
  dateMs: number;
}

const hasPerf = typeof performance !== "undefined" && typeof performance.now === "function";

const sampleAnchor = (): ClockAnchor => ({
  perfMs: hasPerf ? performance.now() : 0,
  dateMs: Date.now(),
});

/**
 * Compute current wall-clock micros from the anchor without taking a
 * fresh `Date.now()` reading — the perf clock is monotonic so this
 * absorbs NTP jumps + manual time-set within one re-anchor window.
 */
const wallClockMicrosFromAnchor = (anchor: ClockAnchor): number => {
  const elapsed = hasPerf ? performance.now() - anchor.perfMs : 0;
  return (anchor.dateMs + elapsed) * 1000;
};

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
  const onOneShot = (beats: number): void =>
    submit(client, { EffectOneShot: { deck, slot, beats } });

  // Live countdown for an in-flight one-shot. Uses an anchored
  // `performance.now()` so the rAF tick is monotonic + sub-ms.
  // The anchor itself re-syncs every 500 ms from `Date.now()` so a
  // long-running tab eventually picks up any NTP correction.
  const oneShot = state.one_shot ?? null;
  const anchorRef = useRef<ClockAnchor>(sampleAnchor());
  const [nowUs, setNowUs] = useState((): number =>
    wallClockMicrosFromAnchor(anchorRef.current),
  );
  useEffect((): (() => void) | void => {
    if (!oneShot) return;
    // Fresh anchor on each new one-shot — the start of a fresh
    // engagement is the right moment to align with the engine's
    // wall clock.
    anchorRef.current = sampleAnchor();
    let raf = 0;
    const resyncTimer = setInterval((): void => {
      anchorRef.current = sampleAnchor();
    }, 500);
    const tick = (): void => {
      setNowUs(wallClockMicrosFromAnchor(anchorRef.current));
      raf = requestAnimationFrame(tick);
    };
    tick();
    return (): void => {
      cancelAnimationFrame(raf);
      clearInterval(resyncTimer);
    };
  }, [oneShot]);
  const remainingMs = oneShot
    ? Math.max(0, Math.round((oneShot.ends_at_micros - nowUs) / 1000))
    : 0;

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
        <div
          style={oneShotRowStyle}
          data-testid={`fx-oneshot-row-${deck}-${slot}`}
          aria-label={`one-shot-deck-${deck}-slot-${slot}`}
        >
          <span style={oneShotLabelStyle}>1-SHOT</span>
          {ONE_SHOT_BEAT_PRESETS.map(
            (beats): JSX.Element => (
              <Button
                key={beats}
                onClick={(): void => onOneShot(beats)}
                pressed={
                  oneShot !== null && remainingMs > 0 && beats === remainingMs
                }
                testId={`fx-oneshot-${deck}-${slot}-${beats}`}
                ariaLabel={`one-shot-${beats}-beats-deck-${deck}-slot-${slot}`}
                style={oneShotBtnStyle}
              >
                {beats}
              </Button>
            ),
          )}
          {oneShot && remainingMs > 0 ? (
            <span
              style={countdownStyle}
              data-testid={`fx-oneshot-countdown-${deck}-${slot}`}
              aria-live="polite"
            >
              {remainingMs} ms
            </span>
          ) : null}
        </div>
      ) : null}

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
