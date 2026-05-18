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

import { useEffect, useMemo } from "react";
import { Deck } from "./Deck";
import { Crossfader } from "./Crossfader";
import { JsonRpcWS } from "../ws/client";
import { applyNotification, useEngineState } from "../store/engine";

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

export const DeckRow = (): JSX.Element => {
  const client = useMemo<JsonRpcWS>(
    () => new JsonRpcWS({ url: wsUrl(), token: token() }),
    [],
  );
  const state = useEngineState();

  useEffect((): (() => void) => {
    const unsubscribe = client.subscribe(applyNotification);
    client.connect();
    return (): void => {
      unsubscribe();
      client.close();
    };
  }, [client]);

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

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100vh",
        background: "#000",
      }}
    >
      <div style={{ display: "flex", flex: 1 }}>
        <Deck deck={state.decks[0]} side="left" client={client} />
        <Deck deck={state.decks[1]} side="right" client={client} />
      </div>
      <Crossfader client={client} value={state.crossfader} />
    </div>
  );
};
