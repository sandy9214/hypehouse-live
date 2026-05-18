// Single-deck panel (ADR-002: 2-deck primitive).
//
// Read-only for this PR — the engine is the source of truth. Future
// PRs will wire MIDI + click handlers that emit submit_event calls.

import type { Deck as DeckState } from "../store/engine";
import { Waveform } from "./Waveform";

export interface DeckProps {
  deck: DeckState;
  side: "left" | "right";
}

const fmtMs = (ms: number): string => {
  const totalSec = Math.floor(ms / 1000);
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
};

const fmtNum = (n: number | null, digits = 1): string =>
  n === null ? "—" : n.toFixed(digits);

const HOT_CUE_COUNT = 8;

export const Deck = ({ deck, side }: DeckProps): JSX.Element => {
  return (
    <section
      aria-label={`Deck ${deck.id}`}
      data-testid={`deck-${deck.id}`}
      style={{
        flex: 1,
        padding: 12,
        borderLeft: side === "right" ? "1px solid #333" : undefined,
        borderRight: side === "left" ? "1px solid #333" : undefined,
        color: "#ddd",
        background: "#1a1a1a",
        fontFamily: "monospace",
      }}
    >
      <header
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "baseline",
        }}
      >
        <h2 style={{ margin: 0, fontSize: 18 }}>Deck {deck.id}</h2>
        <span aria-label="play-state">{deck.playing ? "PLAY" : "PAUSE"}</span>
      </header>

      <div style={{ marginTop: 6 }}>
        <span aria-label="track-title">{deck.track_title ?? "—"}</span>
      </div>

      <Waveform />

      <dl
        style={{
          display: "grid",
          gridTemplateColumns: "max-content 1fr",
          gap: "2px 8px",
          marginTop: 8,
        }}
      >
        <dt>BPM</dt>
        <dd>{fmtNum(deck.bpm, 2)}</dd>
        <dt>Position</dt>
        <dd>{fmtMs(deck.position_ms)}</dd>
        <dt>Pitch</dt>
        <dd>{deck.pitch_semitones.toFixed(2)} st</dd>
        <dt>EQ Low</dt>
        <dd>{deck.eq_low.toFixed(2)}</dd>
        <dt>EQ Mid</dt>
        <dd>{deck.eq_mid.toFixed(2)}</dd>
        <dt>EQ High</dt>
        <dd>{deck.eq_high.toFixed(2)}</dd>
        <dt>Loop</dt>
        <dd>
          {deck.loop_in_ms === null || deck.loop_out_ms === null
            ? "—"
            : `${fmtMs(deck.loop_in_ms)} → ${fmtMs(deck.loop_out_ms)}`}
        </dd>
        <dt>Co-pilot</dt>
        <dd>{deck.copilot_enabled ? "ON" : "OFF"}</dd>
      </dl>

      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(8, 1fr)",
          gap: 4,
          marginTop: 6,
        }}
        aria-label="hot-cues"
      >
        {Array.from({ length: HOT_CUE_COUNT }, (_, i): JSX.Element => {
          const cueMs = deck.hot_cues[i] ?? null;
          return (
            <div
              key={i}
              data-testid={`cue-${deck.id}-${i}`}
              style={{
                padding: "4px 6px",
                fontSize: 11,
                background: cueMs === null ? "#222" : "#2a3f5a",
                textAlign: "center",
                border: "1px solid #333",
              }}
            >
              {i + 1}
              <br />
              {cueMs === null ? "—" : fmtMs(cueMs)}
            </div>
          );
        })}
      </div>
    </section>
  );
};
