// Interactive single-deck panel (ADR-002: 2-deck primitive). Each
// control fires `engine.submit_event` with an externally-tagged
// EventKind payload (engine/src/state.rs). The server's broadcast
// reconciles state — see GH #27 for spec/engine field-name drift notes.
//
// Sub-rows (transport buttons, knob row, hot-cue grid) live in
// DeckControls.tsx; this file owns the layout + RPC-emit wiring.

import { useEffect, useMemo, useState } from "react";
import type { CSSProperties, DragEvent as ReactDragEvent, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { Deck as DeckState } from "../store/engine";
import { extrapolatedPosition, setDeckDuration } from "../store/engine";
import type { LibraryTrack } from "../store/library";
import { useAutoMix } from "../store/autoMix";
import { AutoMixButton } from "./AutoMixButton";
import { CueCountdown } from "./CueCountdown";
import {
  Waveform,
  WAVEFORM_DEFAULT_HEIGHT,
  WAVEFORM_DEFAULT_WIDTH,
} from "./Waveform";
import { HotCueMarkers } from "./HotCueMarkers";
import {
  fmtMs,
  formatEffectiveBpm,
  HotCueGrid,
  KnobRow,
  tempoPctToRatio,
  TransportRow,
  type EqBand,
} from "./DeckControls";
import { EffectRack } from "./EffectRack";
import { LoopBarPresets } from "./LoopBarPresets";
import { StemRack } from "./StemRack";
import { useEffectsManifest } from "../store/effectsManifest";
import {
  getLoadedTrack,
  noteLoadedTrack,
  recordHotCueSet,
  subscribeLoadedTrack,
} from "../store/hotCuePersist";
import { useWaveform } from "../store/waveform";
import { useLibrary } from "../store/library";
import { requestStems, useStemStatus } from "../store/stems";

export interface DeckProps {
  deck: DeckState;
  side: "left" | "right";
  client: JsonRpcWS;
}

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
  // Subscribe to the auto-mix state machine for this deck. The hook
  // re-renders only when the per-deck snapshot reference changes.
  const autoMixSnapshot = useAutoMix(deck.id);

  // Track the library track id currently bound to this deck so we can
  // fetch peaks. `noteLoadedTrack` (fired from the drop handler below)
  // pushes the id into a module-scope map + notifies subscribers; we
  // mirror it here as React state so the Waveform re-renders.
  const [loadedTrackId, setLoadedTrackId] = useState<string | null>(
    (): string | null => getLoadedTrack(deck.id),
  );
  useEffect((): (() => void) => {
    // Initial sync — covers the "deck was loaded before this component
    // mounted" case (drag-load → re-render before first Effect runs).
    setLoadedTrackId(getLoadedTrack(deck.id));
    return subscribeLoadedTrack((d, t): void => {
      if (d === deck.id) setLoadedTrackId(t);
    });
  }, [deck.id]);
  // When the engine reports an empty deck (track unloaded by some other
  // surface), clear our id so the Waveform falls back to flat. We can't
  // detect "engine cleared the deck" any other way — the engine state
  // mirror has no track_id field.
  useEffect((): void => {
    if (!loaded && loadedTrackId !== null) setLoadedTrackId(null);
  }, [loaded, loadedTrackId]);
  const peaks = useWaveform(client, loadedTrackId);
  // Resolve the full LibraryTrack for the deck's currently-loaded id
  // by indexing the library cache. This is the same source TrackRow
  // loaded from, so the bpm / beatgrid / hot_cues we ship in a
  // subsequent `DeckLoadStems` event match the original `DeckLoad`
  // payload bit-for-bit. Falls back to `null` when (a) no track is
  // loaded, or (b) the cache hasn't hydrated yet — the Stems button
  // disables itself in both cases.
  const library = useLibrary(client);
  const loadedTrackMeta = useMemo<LibraryTrack | null>(
    (): LibraryTrack | null => {
      if (loadedTrackId === null) return null;
      return (
        library.tracks.find(
          (t: LibraryTrack): boolean => t.id === loadedTrackId,
        ) ?? null
      );
    },
    [loadedTrackId, library.tracks],
  );
  // Stem-cache status for whatever track is currently loaded. Null
  // trackId = inert hook (no polling). When the user clicks "Stems"
  // we kick `library.compute_stems`; the hook flips status to
  // `"pending"` → `"ready"` on its 2s cadence, then we wire the
  // four paths into the engine via `DeckLoadStems`.
  const stemStatus = useStemStatus(client, loadedTrackId);
  // Track whether the user has *asked* for stems on this deck — we
  // don't want to auto-load stems for every track-cached-on-disk that
  // happens to already have ready stems sitting in the library (that
  // would silently flip mode without consent).
  const [stemsRequested, setStemsRequested] = useState<boolean>(false);
  // Reset request flag whenever the deck unloads — fresh track, fresh
  // intent. Stays sticky during in-flight pending → ready transition.
  useEffect((): void => {
    if (!loaded) setStemsRequested(false);
  }, [loaded]);
  // Auto-emit `DeckLoadStems` once the cache flips to `ready` AFTER
  // the user clicked the button. Using `useEffect` (not the
  // requestStems callback) gives us a single emit per ready-flip,
  // even if the hook re-renders mid-poll.
  useEffect((): void => {
    if (
      !stemsRequested ||
      stemStatus.status !== "ready" ||
      stemStatus.paths === null ||
      loadedTrackMeta === null
    ) {
      return;
    }
    submit(client, {
      DeckLoadStems: {
        deck: deck.id,
        track: { id: loadedTrackMeta.id, path: loadedTrackMeta.path },
        // Engine reducer expects `[vocals, drums, bass, other]` —
        // `useStemStatus` already returns them in this order.
        stem_paths: [
          stemStatus.paths[0],
          stemStatus.paths[1],
          stemStatus.paths[2],
          stemStatus.paths[3],
        ],
        bpm: loadedTrackMeta.bpm,
        beat_grid_anchor_ms: loadedTrackMeta.beat_grid_anchor_ms,
        downbeats_ms: Array.from(loadedTrackMeta.downbeats_ms),
        hot_cues: Array.from(loadedTrackMeta.hot_cues),
      },
    });
    // Clear the request flag — a future click can re-trigger.
    setStemsRequested(false);
  }, [stemsRequested, stemStatus, loadedTrackMeta, client, deck.id]);

  const onStems = (): void => {
    if (!loaded || loadedTrackId === null) return;
    setStemsRequested(true);
    // Fire-and-forget — the hook picks up the resulting status flip
    // via its 2s poll. Errors (e.g. demucs not installed) are
    // swallowed for v0.1; a future PR adds a toast.
    void requestStems(client, loadedTrackId).catch((): void => undefined);
  };
  // Track duration (ms) for playhead positioning. Captured at
  // DeckLoad time from the LibraryTrack payload — the engine's deck
  // state mirror doesn't carry duration today (it isn't needed by the
  // audio thread). When deck is unloaded we drop back to 0 so the
  // playhead stops drawing.
  const [durationMs, setDurationMs] = useState<number>(0);
  useEffect((): void => {
    if (!loaded) setDurationMs(0);
  }, [loaded]);
  // Beat-grid metadata captured at DeckLoad — engine state mirror doesn't
  // carry these (they're library-side analyzer output forwarded into the
  // DeckLoad event payload). Kept in component state so the scrolling
  // Waveform can overlay beat + downbeat markers.
  const [beatAnchorMs, setBeatAnchorMs] = useState<number>(0);
  const [beatPeriodMs, setBeatPeriodMs] = useState<number>(0);
  const [downbeatsMs, setDownbeatsMs] = useState<ReadonlyArray<number>>([]);
  useEffect((): void => {
    if (!loaded) {
      setBeatAnchorMs(0);
      setBeatPeriodMs(0);
      setDownbeatsMs([]);
    }
  }, [loaded]);
  // Register duration with the engine store so `extrapolatedPosition`
  // clamps correctly. Re-fires on duration change (track swap).
  useEffect((): void => {
    setDeckDuration(deck.id, durationMs);
  }, [deck.id, durationMs]);

  // rAF position provider: read freshly from the store every frame so
  // the playhead stays smooth between sparse `state_changed` pushes.
  // Memoised — the Waveform latches it into a ref internally.
  const positionProvider = useMemo(
    (): (() => number) => (): number => extrapolatedPosition(deck.id, durationMs),
    [deck.id, durationMs],
  );

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
  const onTempoPct = (pct: number): void =>
    // Convert the knob's UX-friendly percent into the engine's
    // `tempo_ratio` field. Engine clamps to [0.5, 2.0]; the knob's
    // ±8 % range never approaches that boundary so this is purely
    // defence-in-depth.
    submit(client, {
      TempoBend: { deck: deck.id, ratio: tempoPctToRatio(pct) },
    });
  const onEq = (band: EqBand, value_db: number): void =>
    submit(client, { EqAdjust: { deck: deck.id, band, value_db } });
  const onHotCueTrigger = (slot: number): void =>
    submit(client, { HotCueTrigger: { deck: deck.id, slot } });
  const onHotCueSet = (slot: number): void => {
    // Engine-side: record the new cue in `Deck::hot_cues[slot]`.
    submit(client, {
      HotCueSet: { deck: deck.id, slot, position_ms: deck.position_ms },
    });
    // Library-side: schedule a debounced `library.set_hot_cues` write
    // so the cue survives a track reload. We project the *expected*
    // post-event array locally (engine roundtrip is async) rather
    // than waiting for `state_changed` — this keeps rapid pad-set
    // bursts coalescing on a single timer without dropping the most
    // recent slot.
    const updated: Array<number | null> = Array.from(deck.hot_cues);
    if (slot >= 0 && slot < updated.length) {
      updated[slot] = deck.position_ms;
    }
    recordHotCueSet(client, deck.id, updated);
  };
  const onLoopIn = (): void => submit(client, { LoopIn: { deck: deck.id } });
  const onLoopOut = (): void => submit(client, { LoopOut: { deck: deck.id } });
  const onCopilotToggle = (): void =>
    submit(
      client,
      deck.copilot_enabled
        ? { CopilotDisengage: { deck: deck.id } }
        : { CopilotEngage: { deck: deck.id } },
    );

  // Native HTML5 drop target for a library row drag-source (see
  // TrackRow.tsx). The dataTransfer payload is a serialized
  // LibraryTrack — we parse, then submit a DeckLoad event with the
  // analyzed BPM / anchor / downbeats so the engine can mix without
  // re-asking the library.
  const onDragOver = (e: ReactDragEvent<HTMLElement>): void => {
    if (e.dataTransfer.types.includes("application/x-hypehouse-track")) {
      e.preventDefault();
      e.dataTransfer.dropEffect = "copy";
    }
  };
  const onDrop = (e: ReactDragEvent<HTMLElement>): void => {
    const raw = e.dataTransfer.getData("application/x-hypehouse-track");
    if (!raw) return;
    e.preventDefault();
    let track: LibraryTrack;
    try {
      track = JSON.parse(raw) as LibraryTrack;
    } catch {
      return; // malformed payload — silently ignore
    }
    submit(client, {
      DeckLoad: {
        deck: deck.id,
        track: { id: track.id, path: track.path },
        bpm: track.bpm,
        beat_grid_anchor_ms: track.beat_grid_anchor_ms,
        downbeats_ms: track.downbeats_ms,
        // Library-saved 8-slot hot-cue grid (hot-cue persistence PR).
        // Materialise the readonly slice so the JSON wire shape is a
        // plain mutable array — engine deserialiser doesn't care
        // about TS readonly markers but the runtime serializer might
        // attach extra metadata if we hand it the proxy directly.
        hot_cues: Array.from(track.hot_cues),
      },
    });
    // Tell the hot-cue persistence bridge which library row this deck
    // is now bound to. Subsequent `HotCueSet` events for this deck
    // will debounce-write back to this `track_id`.
    noteLoadedTrack(deck.id, track.id);
    // Capture duration locally so the Waveform playhead can position
    // itself. Engine state mirror doesn't carry duration — it isn't
    // needed by the audio thread, only by the visualiser.
    setDurationMs(Math.round((track.duration_s ?? 0) * 1000));
    // Snapshot beat-grid metadata for the scrolling waveform overlay.
    // These are library-side analyzer outputs not echoed in the engine
    // state mirror; the visualiser is the only consumer.
    setBeatAnchorMs(track.beat_grid_anchor_ms);
    setBeatPeriodMs(track.beat_period_ms);
    setDownbeatsMs(track.downbeats_ms);
  };

  return (
    <section
      aria-label={`Deck ${deck.id}`}
      data-testid={`deck-${deck.id}`}
      style={sectionStyle(side)}
      onDragOver={onDragOver}
      onDrop={onDrop}
    >
      <header style={headerStyle}>
        <h2 style={{ margin: 0, fontSize: 18 }}>Deck {deck.id}</h2>
        <span aria-label="play-state">{deck.playing ? "PLAY" : "PAUSE"}</span>
      </header>

      <div
        aria-label="track-title"
        style={{ display: "flex", alignItems: "center", gap: 8 }}
      >
        <span style={{ flex: 1, minWidth: 0 }}>
          {deck.track_title ?? (
            <span
              data-testid={`deck-${deck.id}-empty-hint`}
              style={{ opacity: 0.6 }}
            >
              Pick a track from the library ↓ (or drop one here)
            </span>
          )}
        </span>
        {/*
          Stems action — kicks `library.compute_stems` for the loaded
          track, then on the next ready-flip auto-emits
          `DeckLoadStems`. Disabled until the deck has a loaded library
          track to operate on (we need its bpm + beatgrid + hot_cues
          for the stem-load event payload). Label flips during the
          compute → ready window so the user sees progress without a
          spinner asset.
        */}
        <button
          type="button"
          aria-label={`Load stems on deck ${deck.id}`}
          data-testid={`stems-load-${deck.id}`}
          disabled={!loaded || loadedTrackMeta === null || stemsRequested}
          onClick={onStems}
          style={{
            background: "#222",
            color: "#ddd",
            border: "1px solid #444",
            borderRadius: 4,
            padding: "2px 8px",
            cursor:
              !loaded || loadedTrackMeta === null ? "not-allowed" : "pointer",
            fontFamily: "monospace",
            fontSize: 12,
          }}
        >
          {stemsRequested
            ? stemStatus.status === "ready"
              ? "Stems ✓"
              : stemStatus.status === "failed"
                ? "Stems ✗"
                : "Stems…"
            : "Stems"}
        </button>
      </div>

      <CueCountdown
        deck={deck.id}
        downbeatsMs={downbeatsMs}
        beatPeriodMs={beatPeriodMs}
        positionProvider={positionProvider}
      />

      <div style={{ position: "relative", display: "inline-block" }}>
        <Waveform
          peaks={peaks}
          positionMs={deck.position_ms}
          durationMs={durationMs}
          mode="scroll"
          beatGridAnchorMs={beatAnchorMs}
          beatPeriodMs={beatPeriodMs}
          downbeatsMs={downbeatsMs}
          positionProvider={positionProvider}
        />
        <HotCueMarkers
          hotCues={deck.hot_cues}
          durationMs={durationMs}
          width={WAVEFORM_DEFAULT_WIDTH}
          height={WAVEFORM_DEFAULT_HEIGHT}
          mode="scroll"
          positionMs={deck.position_ms}
          positionProvider={positionProvider}
          deck={deck.id}
          client={client}
        />
      </div>

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

      <LoopBarPresets deck={deck} client={client} />

      <AutoMixButton
        deck={deck.id}
        client={client}
        snapshot={autoMixSnapshot}
      />

      <KnobRow
        deck={deck}
        onPitch={onPitch}
        onTempoPct={onTempoPct}
        onEq={onEq}
      />

      <StemRack
        deck={deck.id}
        stemGains={deck.stem_gains}
        stemMode={deck.stem_mode}
        client={client}
      />

      <dl style={dlStyle}>
        <dt>BPM</dt>
        <dd data-testid={`bpm-${deck.id}`}>
          {((): JSX.Element => {
            // Show the *effective* BPM (bpm × tempo_ratio) alongside a
            // small "±delta" marker so DJs can see at a glance how far
            // the deck has drifted from its nominal tempo.
            const { effective, delta } = formatEffectiveBpm(
              deck.bpm,
              deck.tempo_ratio,
            );
            return (
              <>
                <span>{effective}</span>
                {delta !== null ? (
                  <span
                    data-testid={`bpm-delta-${deck.id}`}
                    style={{ marginLeft: 6, color: "#8aa", fontSize: 11 }}
                    aria-label={`bpm-delta-${deck.id}`}
                  >
                    {delta}
                  </span>
                ) : null}
              </>
            );
          })()}
        </dd>
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
