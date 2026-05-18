// Auto-Mix toggle button — pulses when active, shows a live countdown
// during the look-ahead window.
//
// Sits next to the CO-PILOT button in `Deck.tsx`'s transport row. Clicks
// fire `copilot.set_auto_mix`; the visual is driven by the `useAutoMix`
// hook (see `store/autoMix.ts`).
//
// Visual rules:
//   * Off: dim gray pill labelled "AUTO-MIX OFF".
//   * On + idle: blue pill, gentle pulse animation.
//   * On + armed: brighter pulse, "ARMING…" label.
//   * On + transitioning: full bright, "MIXING IN Ns" label.
//   * On + countdown active (seconds_to_mix not null): label appends
//     the countdown so the operator knows when the next mix fires.

import type { CSSProperties, JSX } from "react";
import { Button } from "./Button";
import { setAutoMix, type AutoMixSnapshot } from "../store/autoMix";
import type { DeckId } from "../store/engine";
import type { JsonRpcWS } from "../ws/client";

const PULSE_KEYFRAMES_ID = "auto-mix-pulse-keyframes";

/**
 * Inject the @keyframes block once per document. Vite's CSS-in-JS path
 * would be a heavier dependency than the rest of this tree warrants;
 * a single style tag injected on first mount is enough.
 */
const ensureKeyframes = (): void => {
  if (typeof document === "undefined") return;
  if (document.getElementById(PULSE_KEYFRAMES_ID) !== null) return;
  const style = document.createElement("style");
  style.id = PULSE_KEYFRAMES_ID;
  style.textContent = `
@keyframes auto-mix-pulse {
  0%   { box-shadow: 0 0 0 0 rgba(90, 180, 250, 0.6); }
  70%  { box-shadow: 0 0 0 8px rgba(90, 180, 250, 0); }
  100% { box-shadow: 0 0 0 0 rgba(90, 180, 250, 0); }
}
`;
  document.head.appendChild(style);
};

const ledStyle = (status: AutoMixSnapshot["status"]): CSSProperties => ({
  display: "inline-block",
  width: 8,
  height: 8,
  marginRight: 6,
  borderRadius: "50%",
  background:
    status === "transitioning"
      ? "#5fcf6c"
      : status === "armed"
      ? "#f0c75e"
      : status === "done"
      ? "#888"
      : "#5fa0d0",
  verticalAlign: "middle",
});

const buttonExtraStyle = (
  enabled: boolean,
  status: AutoMixSnapshot["status"],
): CSSProperties => ({
  // Pulse only when an actively transitioning / armed deck. The
  // "enabled but idle" case still gets a soft pulse so the operator
  // can tell at a glance which deck is opted in.
  animation: enabled
    ? `auto-mix-pulse ${status === "transitioning" ? "0.8s" : "1.6s"} ease-out infinite`
    : "none",
});

export interface AutoMixButtonProps {
  deck: DeckId;
  client: JsonRpcWS;
  snapshot: AutoMixSnapshot;
}

const formatLabel = (snapshot: AutoMixSnapshot): string => {
  if (!snapshot.enabled) return "AUTO-MIX OFF";
  if (snapshot.status === "transitioning") return "MIXING";
  if (snapshot.status === "armed") {
    return snapshot.seconds_to_mix !== null
      ? `MIX IN ${snapshot.seconds_to_mix}s`
      : "ARMED";
  }
  if (snapshot.seconds_to_mix !== null) {
    return `MIX IN ${snapshot.seconds_to_mix}s`;
  }
  return "AUTO-MIX ON";
};

export const AutoMixButton = ({
  deck,
  client,
  snapshot,
}: AutoMixButtonProps): JSX.Element => {
  // Keyframes are global; injection is idempotent.
  ensureKeyframes();
  const label = formatLabel(snapshot);
  const onClick = (): void => {
    // Optimistic via setAutoMix; failure rolls back inside the store.
    void setAutoMix(client, deck, !snapshot.enabled);
  };
  return (
    <Button
      onClick={onClick}
      pressed={snapshot.enabled}
      testId={`auto-mix-${deck}`}
      ariaLabel={`auto-mix-${deck}`}
      title={`Auto-Mix on Deck ${deck}`}
      style={buttonExtraStyle(snapshot.enabled, snapshot.status)}
    >
      <span
        data-testid={`auto-mix-led-${deck}`}
        style={ledStyle(snapshot.status)}
      />
      <span data-testid={`auto-mix-label-${deck}`}>{label}</span>
      {snapshot.enabled && snapshot.seconds_to_mix !== null ? (
        <span
          data-testid={`auto-mix-countdown-${deck}`}
          style={{ display: "none" }}
        >
          {snapshot.seconds_to_mix}
        </span>
      ) : null}
    </Button>
  );
};
