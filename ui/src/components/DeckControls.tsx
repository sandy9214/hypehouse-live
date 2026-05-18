// Sub-components extracted from Deck.tsx to keep that file <250 lines
// per the council component-size guideline. Stateless, prop-driven —
// they don't know about the JsonRpcWS client; the parent threads
// callbacks for each event.

import type { CSSProperties, JSX } from "react";
import type { Deck as DeckState, DeckId } from "../store/engine";
import { Button } from "./Button";
import { Knob } from "./Knob";

export type EqBand = "Low" | "Mid" | "High";

export const HOT_CUE_COUNT = 8;
const EQ_BANDS: ReadonlyArray<{ band: EqBand; label: string }> = [
  { band: "Low", label: "LOW" },
  { band: "Mid", label: "MID" },
  { band: "High", label: "HIGH" },
];

export const fmtMs = (ms: number): string => {
  const totalSec = Math.floor(ms / 1000);
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
};

const fmtDb = (v: number): string => `${v.toFixed(1)} dB`;
const fmtSt = (v: number): string => `${v.toFixed(1)} st`;

const rowStyle: CSSProperties = { display: "flex", gap: 6, alignItems: "center", flexWrap: "wrap" };
const knobRowStyle: CSSProperties = { display: "flex", gap: 12, alignItems: "flex-end" };
const hotCueGrid: CSSProperties = { display: "grid", gridTemplateColumns: "repeat(4, 1fr)", gap: 4 };
const padStyle: CSSProperties = { padding: "4px 6px", fontSize: 11 };

const ledStyle = (on: boolean): CSSProperties => ({
  display: "inline-block",
  width: 8,
  height: 8,
  marginRight: 6,
  borderRadius: "50%",
  background: on ? "#5fcf6c" : "#444",
  verticalAlign: "middle",
});

const eqValueFor = (d: DeckState, band: EqBand): number =>
  band === "Low" ? d.eq_low : band === "Mid" ? d.eq_mid : d.eq_high;

export const keyHint = (id: DeckId): string => (id === "A" ? "(q)" : "(p)");

export interface TransportRowProps {
  deck: DeckState;
  loaded: boolean;
  hasLoopIn: boolean;
  looping: boolean;
  onPlayPause: () => void;
  onCue: () => void;
  onLoopIn: () => void;
  onLoopOut: () => void;
  onCopilotToggle: () => void;
}

export const TransportRow = ({
  deck, loaded, hasLoopIn, looping,
  onPlayPause, onCue, onLoopIn, onLoopOut, onCopilotToggle,
}: TransportRowProps): JSX.Element => (
  <div style={rowStyle}>
    <Button
      onClick={onPlayPause}
      pressed={deck.playing}
      disabled={!loaded}
      testId={`play-${deck.id}`}
      ariaLabel={`play-pause-${deck.id}`}
      title={`Play/Pause Deck ${deck.id} ${keyHint(deck.id)}`}
    >
      {deck.playing ? "PAUSE" : "PLAY"} {keyHint(deck.id)}
    </Button>
    <Button onClick={onCue} disabled={!loaded} testId={`cue-${deck.id}`} ariaLabel={`cue-${deck.id}`}>
      CUE
    </Button>
    <Button
      onClick={onLoopIn}
      pressed={hasLoopIn && !looping}
      testId={`loop-in-${deck.id}`}
      ariaLabel={`loop-in-${deck.id}`}
    >
      LOOP IN
    </Button>
    <Button
      onClick={onLoopOut}
      pressed={looping}
      disabled={!hasLoopIn}
      testId={`loop-out-${deck.id}`}
      ariaLabel={`loop-out-${deck.id}`}
    >
      LOOP OUT
    </Button>
    <Button
      onClick={onCopilotToggle}
      pressed={deck.copilot_enabled}
      testId={`copilot-${deck.id}`}
      ariaLabel={`copilot-${deck.id}`}
    >
      <span data-testid={`copilot-led-${deck.id}`} style={ledStyle(deck.copilot_enabled)} />
      CO-PILOT {deck.copilot_enabled ? "ON" : "OFF"}
    </Button>
  </div>
);

export interface KnobRowProps {
  deck: DeckState;
  onPitch: (v: number) => void;
  onEq: (band: EqBand, v: number) => void;
}

export const KnobRow = ({ deck, onPitch, onEq }: KnobRowProps): JSX.Element => (
  <div style={knobRowStyle}>
    <Knob
      label="PITCH"
      min={-12}
      max={12}
      step={0.1}
      value={deck.pitch_semitones}
      onChange={onPitch}
      testId={`pitch-${deck.id}`}
      ariaLabel={`pitch-${deck.id}`}
      format={fmtSt}
    />
    {EQ_BANDS.map((b): JSX.Element => (
      <Knob
        key={b.band}
        label={b.label}
        min={-26}
        max={12}
        step={0.5}
        value={eqValueFor(deck, b.band)}
        onChange={(v): void => onEq(b.band, v)}
        testId={`eq-${b.band.toLowerCase()}-${deck.id}`}
        ariaLabel={`eq-${b.band.toLowerCase()}-${deck.id}`}
        format={fmtDb}
      />
    ))}
  </div>
);

export interface HotCueGridProps {
  deck: DeckState;
  loaded: boolean;
  onTrigger: (slot: number) => void;
  onSet: (slot: number) => void;
}

export const HotCueGrid = ({ deck, loaded, onTrigger, onSet }: HotCueGridProps): JSX.Element => (
  <div
    style={hotCueGrid}
    aria-label={`hot-cues-${deck.id}`}
    data-testid={`hot-cues-${deck.id}`}
  >
    {Array.from({ length: HOT_CUE_COUNT }, (_, i): JSX.Element => {
      const cueMs = deck.hot_cues[i] ?? null;
      const hasCue = cueMs !== null;
      return (
        <Button
          key={i}
          onClick={(): void => onTrigger(i)}
          onLongPress={(): void => onSet(i)}
          disabled={!loaded}
          pressed={hasCue}
          testId={`cue-${deck.id}-${i}`}
          ariaLabel={`hot-cue-${deck.id}-${i + 1}`}
          style={padStyle}
        >
          {i + 1}
          <br />
          {hasCue ? fmtMs(cueMs ?? 0) : "—"}
        </Button>
      );
    })}
  </div>
);
