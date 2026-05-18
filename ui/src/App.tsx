// hypehouse-live root component.
//
// Hosts the 2-deck UI (ADR-002). Layout is intentionally bare so
// later styling PRs can swap in the real shell.

import { DeckRow } from "./components/DeckRow";

export const App = (): JSX.Element => {
  return (
    <main aria-label="hypehouse-live root">
      <DeckRow />
    </main>
  );
};
