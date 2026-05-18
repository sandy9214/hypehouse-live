// Interactive single-deck panel (ADR-002: 2-deck primitive). Each
// control fires `engine.submit_event` with an externally-tagged
// EventKind payload (engine/src/state.rs). The server's broadcast
// reconciles state — see GH #27 for spec/engine field-name drift notes.
//
// Sub-rows (transport buttons, knob row, hot-cue grid) live in
// DeckControls.tsx; this file owns the layout + RPC-emit wiring.

import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { Deck as DeckState } from "../store/engine";
import { Waveform } from "./Waveform";
import {
  fmtMs,
  HotCueGrid,
  KnobRow,
  TransportRow,
  type EqBand,
} from "./DeckControls";
import { EffectRack } from "./EffectRack";
import { useEffectsManifest } from "../store/effectsManifest";

export interface DeckProps {
  deck: DeckState;
  side: "left" | "right";
  client: JsonRpcWS;
}

const fmtNum = (n: number | null, digits = 1): string =>
  n === null ? "—" : n.toFixed(digits);
const isLoaded = (d: DeckState): boolean => d.track_title !== null;
const loopActive = (d: DeckState): boolean =>
  d.loop_in_ms !== null && d.loop_out_ms !== null;

const submit = (client: JsonRpcWS, payload: Record<string, unknown>): void => {
  // Fire-and-forget; v0.1 swallows errors (later PR adds a toast layer).
  void client.call("submit_event", payload).catch((): void => undefined);
};

const sectionStyle = (side: "left" | "right"): CSSProperties => ({
  flex: 1,
  padding: 12,
  borderLeft: side === "right" ? "1px solid #333" : undefined,
  borderRight: side === "left" ? "1px solid #333" : undefined,
  color: "#ddd",
  background: "#1a1a1a",
  fontFamily: "monospace",
  display: "flex",
  flexDirection: "column",
  gap: 8,
});

const headerStyle: CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  alignItems: "baseline",
};

const dlStyle: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "max-content 1fr",
  gap: "2px 8px",
  margin: 0,
  fontSize: 12,
};

export const Deck = ({ deck, side, client }: DeckProps): JSX.Element => {
  const loaded = isLoaded(deck);
  const hasLoopIn = deck.loop_in_ms !== null;
  const looping = loopActive(deck);
  const manifest = useEffectsManifest(client);

  const onPlayPause = (): void => {
    if (!loaded) return;
    submit(
      client,
      deck.playing
        ? { DeckPause: { deck: deck.id } }
        : { DeckPlay: { deck: deck.id } },
    );
  };
  const onCue = (): void =>
    submit(client, {
      DeckCue: { deck: deck.id, position_ms: deck.position_ms },
    });
  const onPitch = (value: number): void =>
    submit(client, { PitchBend: { deck: deck.id, semitones: value } });
  const onEq = (band: EqBand, value_db: number): void =>
    submit(client, { EqAdjust: { deck: deck.id, band, value_db } });
  const onHotCueTrigger = (slot: number): void =>
    submit(client, { HotCueTrigger: { deck: deck.id, slot } });
  const onHotCueSet = (slot: number): void =>
    submit(client, {
      HotCueSet: { deck: deck.id, slot, position_ms: deck.position_ms },
    });
  const onLoopIn = (): void => submit(client, { LoopIn: { deck: deck.id } });
  const onLoopOut = (): void => submit(client, { LoopOut: { deck: deck.id } });
  const onCopilotToggle = (): void =>
    submit(
      client,
      deck.copilot_enabled
        ? { CopilotDisengage: { deck: deck.id } }
        : { CopilotEngage: { deck: deck.id } },
    );

  return (
    <section
      aria-label={`Deck ${deck.id}`}
      data-testid={`deck-${deck.id}`}
      style={sectionStyle(side)}
    >
      <header style={headerStyle}>
        <h2 style={{ margin: 0, fontSize: 18 }}>Deck {deck.id}</h2>
        <span aria-label="play-state">{deck.playing ? "PLAY" : "PAUSE"}</span>
      </header>

      <div aria-label="track-title">{deck.track_title ?? "—"}</div>

      <Waveform />

      <TransportRow
        deck={deck}
        loaded={loaded}
        hasLoopIn={hasLoopIn}
        looping={looping}
        onPlayPause={onPlayPause}
        onCue={onCue}
        onLoopIn={onLoopIn}
        onLoopOut={onLoopOut}
        onCopilotToggle={onCopilotToggle}
      />

      <KnobRow deck={deck} onPitch={onPitch} onEq={onEq} />

      <dl style={dlStyle}>
        <dt>BPM</dt>
        <dd>{fmtNum(deck.bpm, 2)}</dd>
        <dt>Pos</dt>
        <dd>{fmtMs(deck.position_ms)}</dd>
        <dt>Loop</dt>
        <dd>
          {looping
            ? `${fmtMs(deck.loop_in_ms ?? 0)} → ${fmtMs(deck.loop_out_ms ?? 0)}`
            : hasLoopIn
              ? `${fmtMs(deck.loop_in_ms ?? 0)} → …`
              : "—"}
        </dd>
      </dl>

      <HotCueGrid
        deck={deck}
        loaded={loaded}
        onTrigger={onHotCueTrigger}
        onSet={onHotCueSet}
      />

      <EffectRack
        deck={deck.id}
        effects={deck.effects}
        manifest={manifest}
        client={client}
      />
    </section>
  );
};
