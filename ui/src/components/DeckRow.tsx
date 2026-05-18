// DeckRow — two-deck layout (A left, B right) with crossfader.
//
// Composition root for the 2-deck primitive (ADR-002). Subscribes to
// the engine state store via `useEngineState` and forwards the shared
// JSON-RPC client down to each Deck so the interactive controls can
// emit `submit_event`.
//
// Keyboard shortcuts (browser-only — proper MIDI lives in src/midi):
//   - "q" toggles Deck A play/pause
//   - "p" toggles Deck B play/pause
// The MIDI keyboard mapping is the source of truth for production
// use; this is a convenience overlay for the desktop browser preview.

import { useEffect, useMemo, useState } from "react";
import { Deck } from "./Deck";
import { Crossfader } from "./Crossfader";
import { MasterControls } from "./MasterControls";
import { MobileDeckSwiper } from "./MobileDeckSwiper";
import { PerfDashboard } from "./PerfDashboard";
import { PresetPanel } from "./PresetPanel";
import { SecondaryPanel, type SecondaryTab } from "./SecondaryPanel";
import { JsonRpcWS } from "../ws/client";
import { applyNotification, useEngineState } from "../store/engine";
import { applyDecodeErrorNotification } from "../store/notifications";
import { applyPerfNotification } from "../store/perf";
import { useViewport } from "../hooks/useViewport";
import { RESPONSIVE_CSS } from "./responsive.css";

// SecondaryTab type lives with SecondaryPanel — re-exported under the
// same name from this module previously, so existing internal
// references continue to compile.

const wsUrl = (): string => {
  // Vite dev server proxies /ws to the Rust engine; prod build can
  // override via VITE_BRIDGE_URL once we ship Tauri/static hosting.
  const env = (import.meta as { env?: Record<string, string | undefined> })
    .env;
  const override = env?.VITE_BRIDGE_URL;
  if (override) return override;
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}/ws`;
};

const token = (): string => {
  const env = (import.meta as { env?: Record<string, string | undefined> })
    .env;
  return env?.VITE_BRIDGE_TOKEN ?? "dev-token";
};

// Responsive CSS lives in ./responsive.css.ts so DeckRow.tsx stays
// under the 250-line per-component budget. The string is injected
// once at the root of the deck tree via a <style> tag.

export interface DeckRowProps {
  /** Optional shared JSON-RPC client. When provided, DeckRow uses it
   * instead of constructing its own — the lifecycle (subscribe +
   * connect + close) still belongs to DeckRow so existing callers that
   * pass nothing get the legacy self-managed behaviour. Hoisted by
   * `App.tsx` so the onboarding wizard and the deck UI share a single
   * WebSocket. */
  client?: JsonRpcWS;
}

export const DeckRow = ({ client: external }: DeckRowProps = {}): JSX.Element => {
  const client = useMemo<JsonRpcWS>(
    () => external ?? new JsonRpcWS({ url: wsUrl(), token: token() }),
    [external],
  );
  const state = useEngineState();
  const [tab, setTab] = useState<SecondaryTab>("live");
  const viewport = useViewport();
  // Mobile library drawer state. On desktop / tablet the library is a
  // fixed bottom panel; on mobile it overlays the deck on demand to
  // free up real-estate for the active deck. Default closed on first
  // mount — user taps "Library" to reveal.
  const [libraryOpen, setLibraryOpen] = useState<boolean>(false);

  const ownsClient = external === undefined;
  useEffect((): (() => void) => {
    const unsubscribeState = client.subscribe(applyNotification);
    const unsubscribeDecodeErrors = client.subscribe(
      applyDecodeErrorNotification,
    );
    const unsubscribePerf = client.subscribe(applyPerfNotification);
    client.connect();
    return (): void => {
      unsubscribeState();
      unsubscribeDecodeErrors();
      unsubscribePerf();
      // Only close the socket if we constructed it ourselves — when a
      // parent (App.tsx) injected the client the parent owns its
      // lifecycle.
      if (ownsClient) client.close();
    };
  }, [client, ownsClient]);

  useEffect((): (() => void) => {
    const handler = (ev: KeyboardEvent): void => {
      // Ignore when the user is typing into a field.
      const target = ev.target as HTMLElement | null;
      if (
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.isContentEditable)
      ) {
        return;
      }
      const key = ev.key.toLowerCase();
      const isA = key === "q";
      const isB = key === "p";
      if (!isA && !isB) return;
      ev.preventDefault();
      const deck = state.decks[isA ? 0 : 1];
      if (deck.track_title === null) return;
      const payload = deck.playing
        ? { DeckPause: { deck: deck.id } }
        : { DeckPlay: { deck: deck.id } };
      void client.call("submit_event", payload).catch((): void => undefined);
    };
    window.addEventListener("keydown", handler);
    return (): void => {
      window.removeEventListener("keydown", handler);
    };
  }, [client, state]);

  // Viewport-aware deck region. Mobile uses MobileDeckSwiper (single
  // deck + swipe). Tablet stacks the decks vertically. Desktop keeps
  // the existing side-by-side flex row — explicitly the legacy path so
  // the 259 baseline tests (which run in jsdom with default 1024 width)
  // continue to find both decks side-by-side.
  const deckRegion = ((): JSX.Element => {
    if (viewport === "mobile") {
      return (
        <MobileDeckSwiper
          decks={[state.decks[0], state.decks[1]]}
          client={client}
        />
      );
    }
    // Tablet (stacked) + desktop (side-by-side) share the same JSX —
    // CSS media query at 768-1023 px flips the flex-direction via the
    // `.hh-deck-stack` class. Default (desktop) is row.
    return (
      <div
        data-testid="deck-stack"
        className="hh-deck-stack"
        style={{ display: "flex", flex: 1, minHeight: 0 }}
      >
        <Deck deck={state.decks[0]} side="left" client={client} />
        <Deck deck={state.decks[1]} side="right" client={client} />
      </div>
    );
  })();

  const isMobile = viewport === "mobile";

  return (
    <div
      className="hh-responsive-root"
      data-viewport={viewport}
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100vh",
        background: "#000",
      }}
    >
      <style>{RESPONSIVE_CSS}</style>
      {deckRegion}
      <Crossfader client={client} value={state.crossfader} />
      <MasterControls
        client={client}
        enabled={state.master_limiter_enabled}
        thresholdDb={state.master_limiter_threshold_db}
        gainReductionDb={state.master_limiter_gain_reduction_db}
        clockSource={state.clock_source}
      />
      <PerfDashboard />
      <PresetPanel
        client={client}
        decks={state.decks}
        crossfaderCurve={state.crossfader_curve}
      />
      <SecondaryPanel
        client={client}
        tab={tab}
        setTab={setTab}
        isMobile={isMobile}
        libraryOpen={libraryOpen}
        setLibraryOpen={setLibraryOpen}
      />
    </div>
  );
};
