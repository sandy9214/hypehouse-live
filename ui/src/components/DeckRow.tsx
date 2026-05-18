// DeckRow — two-deck layout (A left, B right) with crossfader.
//
// Composition root for the 2-deck primitive (ADR-002). Subscribes to
// the engine state store via `useEngineState`.

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
        <Deck deck={state.decks[0]} side="left" />
        <Deck deck={state.decks[1]} side="right" />
      </div>
      <Crossfader client={client} value={state.crossfader} />
    </div>
  );
};
