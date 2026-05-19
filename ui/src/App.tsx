// hypehouse-live root component.
//
// Hosts the 2-deck UI (ADR-002) plus the first-launch onboarding wizard
// (PR: ui-onboarding-flow). Layout is intentionally bare so later
// styling PRs can swap in the real shell. The `<Toaster />` child
// renders engine-pushed decode-error notifications as toasts in the
// bottom-right corner — it lives at the root so toasts remain visible
// regardless of which secondary tab DeckRow is showing.
//
// Onboarding gating:
//   * `useOnboarding().complete === false` (localStorage flag missing).
//   * The library's `total` is 0 (queried via `library.list_tracks`
//     with `limit:1` — there's no separate `count_tracks` RPC).
//   Either condition false ⇒ no wizard. Once both true, the modal
//   renders on top of the deck UI until the user finishes / cancels /
//   skips. We share a single JSON-RPC client between the wizard and
//   the deck UI to avoid opening two WebSocket sessions on launch.

import { useEffect, useMemo, useState } from "react";
import { DeckRow } from "./components/DeckRow";
import { Onboarding } from "./components/Onboarding";
import { OutputDevicePicker } from "./components/OutputDevicePicker";
import { SidechainPanel } from "./components/SidechainPanel";
import { useEngineState } from "./store/engine";
import { Toaster } from "./components/Toaster";
import { JsonRpcWS } from "./ws/client";
import { useOnboarding } from "./store/onboarding";

const wsUrl = (): string => {
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

interface ListTotal {
  readonly total: number;
}

export const App = (): JSX.Element => {
  const client = useMemo<JsonRpcWS>(
    () => new JsonRpcWS({ url: wsUrl(), token: token() }),
    [],
  );
  const onboarding = useOnboarding();
  // `undefined` while we haven't probed the library yet; `number` once
  // the first `library.list_tracks` lands. Drives the "should the
  // wizard appear?" gate.
  const [libraryTotal, setLibraryTotal] = useState<number | undefined>(
    undefined,
  );
  // User-explicit dismiss within this session — even if the library is
  // still empty, don't keep re-opening the wizard until next launch.
  const [dismissed, setDismissed] = useState<boolean>(false);

  useEffect((): void => {
    if (onboarding.complete) return;
    client
      .call<unknown>("library.list_tracks", { limit: 1, offset: 0 })
      .then((r: unknown): void => {
        if (r && typeof r === "object" && "total" in r) {
          const t = (r as ListTotal).total;
          if (typeof t === "number") setLibraryTotal(t);
        } else {
          setLibraryTotal(0);
        }
      })
      .catch((): void => {
        // Engine offline or RPC error → don't open the wizard; let the
        // user see the regular UI with its error banner. A future
        // retry path can re-probe.
        setLibraryTotal(undefined);
      });
  }, [client, onboarding.complete]);

  const showWizard =
    !onboarding.complete && !dismissed && libraryTotal === 0;

  const engineState = useEngineState();

  return (
    <main aria-label="hypehouse-live root">
      <DeckRow client={client} />
      <aside aria-label="Audio output settings" style={{ padding: "0.5rem 0" }}>
        <OutputDevicePicker client={client} />
      </aside>
      <aside aria-label="Sidechain compressor" style={{ padding: "0.5rem 0" }}>
        <SidechainPanel client={client} state={engineState.sidechain ?? null} />
      </aside>
      <Toaster />
      {showWizard && (
        <Onboarding
          client={client}
          onClose={(completed: boolean): void => {
            if (completed) onboarding.markComplete();
            setDismissed(true);
          }}
        />
      )}
    </main>
  );
};
