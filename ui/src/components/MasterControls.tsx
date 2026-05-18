// MasterControls — master-bus limiter UI (toggle + threshold knob +
// gain-reduction meter).
//
// The engine ships a master-bus soft-clip limiter (see
// `engine/src/audio/limiter.rs`). This component exposes three controls
// next to the crossfader so the user can:
//   1. Bypass the limiter (Button → `SetMasterLimiterEnabled`).
//   2. Tune the threshold from -24 dB to 0 dB (Knob → `SetMasterLimiterThreshold`).
//   3. Watch the live gain reduction (vertical meter, smoothed).
//
// The meter reads `state.master_limiter_gain_reduction_db` (a sibling
// field on each `engine.state_changed` payload, sampled off the audio
// thread's shared atomic — see `engine/src/bridge/rpc.rs`). State_changed
// fires every event, which is "good enough" for a UI meter but is bursty
// — between bursts we coast on the last value. We pair the discrete
// state ticks with a `requestAnimationFrame` loop that exponentially
// decays the rendered value toward the latest sample, so:
//   - a sudden -6 dB GR doesn't snap on screen (would visually pop), and
//   - the meter falls back smoothly to 0 dB when the engine recovers.
// The time constant `METER_TAU_MS` is tuned for ~120 ms 63%-settle —
// snappy enough for a DJ to see transient reductions but slow enough to
// not flicker on noisy single-sample peaks.

import { useCallback, useEffect, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { Button } from "./Button";
import { Knob } from "./Knob";
import type { JsonRpcWS } from "../ws/client";

export interface MasterControlsProps {
  client: JsonRpcWS;
  enabled: boolean;
  thresholdDb: number;
  gainReductionDb: number;
  /** Override the smoothing time-constant (ms). Tests pin it to 0
   * for deterministic snap-to-target meter. */
  meterTauMs?: number;
}

/** Engine-side limiter knob window (mirrors
 * `audio::MASTER_LIMITER_MIN/MAX_THRESHOLD_DB`). */
const THRESHOLD_MIN_DB = -24;
const THRESHOLD_MAX_DB = 0;
const THRESHOLD_STEP_DB = 0.5;

/** Meter scale — 0 dB at the top, -12 dB at the bottom. Real-world
 * master-bus reductions are typically -1 to -6 dB; -12 gives the user
 * visual headroom on the worst-case kick spike. */
const METER_FLOOR_DB = -12;

/** Exponential-decay time-constant (ms) for the meter smoother. */
const METER_TAU_MS_DEFAULT = 120;

const wrap: CSSProperties = {
  display: "flex",
  gap: 12,
  padding: "8px 12px",
  background: "#0e0e0e",
  color: "#ddd",
  alignItems: "center",
  fontFamily: "monospace",
  fontSize: 11,
  borderTop: "1px solid #222",
};

const meterCol: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  alignItems: "center",
  gap: 2,
};

const meterTrackStyle: CSSProperties = {
  position: "relative",
  width: 14,
  height: 80,
  background: "#161616",
  border: "1px solid #333",
  borderRadius: 2,
  overflow: "hidden",
};

const meterReadoutStyle: CSSProperties = {
  fontFamily: "monospace",
  fontSize: 11,
  color: "#ddd",
  width: 56,
  textAlign: "right",
};

/** Map a gain-reduction value (`<= 0`) to a fill ratio in `[0, 1]`.
 * 0 dB → 0 (empty meter); `METER_FLOOR_DB` → 1 (full meter). */
const grToRatio = (db: number): number => {
  if (!Number.isFinite(db) || db >= 0) return 0;
  const r = -db / -METER_FLOOR_DB;
  return r < 0 ? 0 : r > 1 ? 1 : r;
};

const formatThreshold = (db: number): string => `${db.toFixed(1)} dB`;

export const MasterControls = ({
  client,
  enabled,
  thresholdDb,
  gainReductionDb,
  meterTauMs = METER_TAU_MS_DEFAULT,
}: MasterControlsProps): JSX.Element => {
  // Smoothed meter value. We start at the engine's sampled GR so the
  // first paint reflects reality rather than animating up from zero.
  const [displayGr, setDisplayGr] = useState<number>(gainReductionDb);
  // Latest target from the engine — written by the state-tick effect,
  // read by the rAF loop. Ref because rAF closures don't see new state.
  const targetRef = useRef<number>(gainReductionDb);
  const rafRef = useRef<number | null>(null);
  const lastFrameMsRef = useRef<number | null>(null);

  useEffect((): void => {
    targetRef.current = gainReductionDb;
  }, [gainReductionDb]);

  // rAF smoother. Runs while the component is mounted; cheap (one tick
  // per ~16 ms) and stops cleanly on unmount. When tauMs <= 0 we snap
  // instantly — convenient for tests + a future "raw" debug toggle.
  useEffect((): (() => void) => {
    const tick = (now: number): void => {
      const last = lastFrameMsRef.current ?? now;
      const dt = Math.max(0, now - last);
      lastFrameMsRef.current = now;
      const target = targetRef.current;
      setDisplayGr((prev): number => {
        if (meterTauMs <= 0) return target;
        // One-pole low-pass: prev += alpha · (target - prev).
        const alpha = 1 - Math.exp(-dt / meterTauMs);
        const next = prev + alpha * (target - prev);
        // Snap to target inside a JND-safe tolerance so we don't
        // pump endlessly between two near-identical floats.
        return Math.abs(target - next) < 1e-3 ? target : next;
      });
      rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return (): void => {
      if (rafRef.current !== null) cancelAnimationFrame(rafRef.current);
      rafRef.current = null;
      lastFrameMsRef.current = null;
    };
  }, [meterTauMs]);

  const handleToggle = useCallback((): void => {
    void client
      .call("submit_event", { SetMasterLimiterEnabled: { enabled: !enabled } })
      .catch((): void => undefined);
  }, [client, enabled]);

  const handleThreshold = useCallback(
    (next: number): void => {
      void client
        .call("submit_event", {
          SetMasterLimiterThreshold: { threshold_db: next },
        })
        .catch((): void => undefined);
    },
    [client],
  );

  const ratio = grToRatio(displayGr);
  const fillStyle: CSSProperties = {
    position: "absolute",
    left: 0,
    right: 0,
    bottom: 0,
    height: `${(ratio * 100).toFixed(2)}%`,
    background:
      "linear-gradient(to top, #ff3333 0%, #ff9b1a 60%, #ffd84a 100%)",
    transition: "none",
  };

  return (
    <div style={wrap} data-testid="master-controls">
      <Button
        pressed={enabled}
        onClick={handleToggle}
        testId="limiter-toggle"
        ariaLabel="master limiter enabled"
        title={enabled ? "Limiter ON — click to bypass" : "Limiter OFF"}
      >
        LIMITER {enabled ? "ON" : "OFF"}
      </Button>
      <Knob
        label="THRESH"
        min={THRESHOLD_MIN_DB}
        max={THRESHOLD_MAX_DB}
        step={THRESHOLD_STEP_DB}
        value={thresholdDb}
        onChange={handleThreshold}
        resetValue={-0.5}
        size={70}
        format={formatThreshold}
        testId="limiter-threshold"
        ariaLabel="master limiter threshold dB"
      />
      <div style={meterCol}>
        <span aria-hidden="true">GR</span>
        <div
          style={meterTrackStyle}
          role="meter"
          aria-valuemin={METER_FLOOR_DB}
          aria-valuemax={0}
          aria-valuenow={Math.max(METER_FLOOR_DB, Math.min(0, displayGr))}
          aria-label="master limiter gain reduction"
          data-testid="limiter-meter"
        >
          <div style={fillStyle} data-testid="limiter-meter-fill" />
        </div>
      </div>
      <span style={meterReadoutStyle} data-testid="limiter-meter-readout">
        {`${displayGr <= 0 ? displayGr.toFixed(1) : "0.0"} dB GR`}
      </span>
    </div>
  );
};
