// PresetPanel — save / load / delete scene snapshots.
//
// A "preset" captures the controllable surface of both decks
// (3 effect slots, 3 EQ bands, pitch, tempo) plus the master
// crossfader response curve. Loading a preset is a *replay*: we
// dispatch the matching `submit_event` calls so the engine reducer
// applies them like any other UI input. The engine is the source of
// truth; we never mutate the mirror directly.
//
// Replay event sequence per preset load:
//   per-deck (A then B):
//     - per slot (0..2):
//       * `EffectClear` if preset slot is empty;
//       * `EffectAssign` + `EffectParam`* + `EffectWetDry` + `EffectEnable` otherwise.
//     - `EqAdjust` Low / Mid / High.
//     - `PitchBend` + `TempoBend`.
//   `SetCrossfaderCurve`.
//
// Total events for a populated preset = 2 decks × (3 slots × ~4
// events + 3 EQ + 2 pitch/tempo) + 1 curve = ~35 submit_event calls.
// Each call is fire-and-forget; the engine's last_event_id ordering
// stamps them in arrival order. We send them serially so a slow link
// doesn't reorder a slot's Assign vs. its Params.

import { useCallback, useState } from "react";
import type { CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type {
  CrossfaderCurve,
  Deck,
  DeckId,
  EffectSlotState,
} from "../store/engine";
import {
  deletePreset,
  loadPreset,
  savePreset,
  usePresets,
  type Preset,
  type PresetDeckState,
} from "../store/presets";

export interface PresetPanelProps {
  client: JsonRpcWS;
  decks: readonly [Deck, Deck];
  crossfaderCurve: CrossfaderCurve;
  /** Test hook — replace `window.prompt` so save flow is deterministic. */
  promptFn?: (msg: string) => string | null;
  /** Test hook — bypass the destructive-action confirm. */
  confirmFn?: (msg: string) => boolean;
}

const wrap: CSSProperties = {
  background: "#0c0c0c",
  borderTop: "1px solid #333",
  color: "#ddd",
  padding: "6px 8px",
  fontFamily: "monospace",
  fontSize: 12,
  display: "flex",
  flexDirection: "column",
  gap: 6,
};

const headerRow: CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 8,
};

const listStyle: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: 2,
  maxHeight: 160,
  overflowY: "auto",
};

const rowStyle: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "1fr 140px 60px 60px",
  gap: 8,
  padding: "4px 6px",
  background: "#101010",
  border: "1px solid #1c1c1c",
  borderRadius: 3,
  alignItems: "center",
};

const btnStyle: CSSProperties = {
  background: "#1c2a3d",
  color: "#cce0ff",
  border: "1px solid #2c4361",
  borderRadius: 3,
  padding: "3px 8px",
  fontSize: 11,
  fontFamily: "monospace",
  cursor: "pointer",
};

const dangerBtnStyle: CSSProperties = {
  ...btnStyle,
  background: "#3d1c1c",
  color: "#ffd0d0",
  border: "1px solid #61292c",
};

const submit = (
  client: JsonRpcWS,
  payload: Record<string, unknown>,
): Promise<unknown> =>
  client.call("submit_event", payload).catch((): unknown => undefined);

/** Project a Deck mirror slice into the preset-wire shape. */
const deckToPresetState = (d: Deck): PresetDeckState => ({
  effects: d.effects.map(
    (s: EffectSlotState): PresetDeckState["effects"][number] => ({
      effect_id: s.effect_id,
      params: { ...s.params },
      wet_dry: s.wet_dry,
      enabled: s.enabled,
    }),
  ),
  eq_low_db: d.eq_low,
  eq_mid_db: d.eq_mid,
  eq_high_db: d.eq_high,
  pitch_semitones: d.pitch_semitones,
  tempo_ratio: d.tempo_ratio,
});

/**
 * Replay one preset's deck state as a sequence of `submit_event`s.
 * Returns the number of events dispatched so callers / tests can
 * assert the replay scope. Sent serially so an Assign always precedes
 * the matching Params in the engine's event log.
 */
export const replayDeckState = async (
  client: JsonRpcWS,
  deckId: DeckId,
  deck: PresetDeckState,
): Promise<number> => {
  let count = 0;
  for (let slot = 0; slot < deck.effects.length; slot++) {
    const s = deck.effects[slot];
    if (s.effect_id === 0) {
      await submit(client, { EffectClear: { deck: deckId, slot } });
      count += 1;
      continue;
    }
    await submit(client, {
      EffectAssign: { deck: deckId, slot, effect_id: s.effect_id },
    });
    count += 1;
    for (const [param, value] of Object.entries(s.params)) {
      await submit(client, {
        EffectParam: { deck: deckId, slot, param, value },
      });
      count += 1;
    }
    await submit(client, {
      EffectWetDry: { deck: deckId, slot, value: s.wet_dry },
    });
    count += 1;
    await submit(client, {
      EffectEnable: { deck: deckId, slot, enabled: s.enabled },
    });
    count += 1;
  }
  // EQ bands — band names match the engine's `EqBand` enum tags.
  await submit(client, {
    EqAdjust: { deck: deckId, band: "Low", value_db: deck.eq_low_db },
  });
  await submit(client, {
    EqAdjust: { deck: deckId, band: "Mid", value_db: deck.eq_mid_db },
  });
  await submit(client, {
    EqAdjust: { deck: deckId, band: "High", value_db: deck.eq_high_db },
  });
  count += 3;
  await submit(client, {
    PitchBend: { deck: deckId, semitones: deck.pitch_semitones },
  });
  await submit(client, {
    TempoBend: { deck: deckId, ratio: deck.tempo_ratio },
  });
  count += 2;
  return count;
};

/**
 * Replay an entire preset — both decks plus the crossfader curve.
 * Exported so the panel + tests can call the same code path.
 */
export const replayPreset = async (
  client: JsonRpcWS,
  preset: Preset,
): Promise<number> => {
  let count = 0;
  count += await replayDeckState(client, "A", preset.deck_a);
  count += await replayDeckState(client, "B", preset.deck_b);
  await submit(client, {
    SetCrossfaderCurve: { curve: preset.crossfader_curve },
  });
  count += 1;
  return count;
};

export const PresetPanel = ({
  client,
  decks,
  crossfaderCurve,
  promptFn,
  confirmFn,
}: PresetPanelProps): JSX.Element => {
  const snapshot = usePresets(client);
  // `busyId` is the id of the preset currently being loaded / deleted
  // — disables both buttons on the row so a double-click doesn't fire
  // a second pile of submit_events.
  const [busyId, setBusyId] = useState<number | "saving" | null>(null);

  const handleSave = useCallback(async (): Promise<void> => {
    const ask = promptFn ?? ((msg: string): string | null => window.prompt(msg));
    const raw = ask("Preset name?");
    if (raw === null) return;
    const name = raw.trim();
    if (!name) return;
    setBusyId("saving");
    try {
      await savePreset(client, {
        name,
        deck_a: deckToPresetState(decks[0]),
        deck_b: deckToPresetState(decks[1]),
        crossfader_curve: crossfaderCurve,
      });
    } finally {
      setBusyId(null);
    }
  }, [client, decks, crossfaderCurve, promptFn]);

  const handleLoad = useCallback(
    async (id: number): Promise<void> => {
      setBusyId(id);
      try {
        const preset = await loadPreset(client, id);
        if (preset) {
          await replayPreset(client, preset);
        }
      } finally {
        setBusyId(null);
      }
    },
    [client],
  );

  const handleDelete = useCallback(
    async (id: number, name: string): Promise<void> => {
      const ask =
        confirmFn ?? ((msg: string): boolean => window.confirm(msg));
      if (!ask(`Delete preset ${name}?`)) return;
      setBusyId(id);
      try {
        await deletePreset(client, id);
      } finally {
        setBusyId(null);
      }
    },
    [client, confirmFn],
  );

  return (
    <div style={wrap} data-testid="preset-panel">
      <div style={headerRow}>
        <span style={{ opacity: 0.7, textTransform: "uppercase" }}>
          Presets
        </span>
        <button
          type="button"
          style={btnStyle}
          onClick={(): void => {
            void handleSave();
          }}
          disabled={busyId === "saving"}
          data-testid="preset-save"
        >
          Save current
        </button>
        {snapshot.error !== null && (
          <span style={{ color: "#ff8080" }} data-testid="preset-error">
            {snapshot.error}
          </span>
        )}
      </div>
      {snapshot.presets.length === 0 ? (
        <div
          style={{ opacity: 0.55, padding: "8px 4px" }}
          data-testid="preset-empty"
        >
          No saved presets — capture the current scene with "Save current".
        </div>
      ) : (
        <div style={listStyle} data-testid="preset-list">
          {snapshot.presets.map((p) => {
            const rowBusy = busyId === p.id;
            return (
              <div
                key={p.id}
                style={rowStyle}
                data-testid={`preset-row-${p.id}`}
              >
                <span>{p.name}</span>
                <span style={{ opacity: 0.55, fontSize: 11 }}>
                  {p.created_at}
                </span>
                <button
                  type="button"
                  style={btnStyle}
                  disabled={rowBusy}
                  onClick={(): void => {
                    void handleLoad(p.id);
                  }}
                  data-testid={`preset-load-${p.id}`}
                >
                  Load
                </button>
                <button
                  type="button"
                  style={dangerBtnStyle}
                  disabled={rowBusy}
                  onClick={(): void => {
                    void handleDelete(p.id, p.name);
                  }}
                  data-testid={`preset-delete-${p.id}`}
                >
                  Delete
                </button>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
};
