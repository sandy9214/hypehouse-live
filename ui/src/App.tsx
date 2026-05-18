// hypehouse-live root component.
//
// Hosts the 2-deck UI (ADR-002). Layout is intentionally bare so
// later styling PRs can swap in the real shell. The `<Toaster />`
// child renders engine-pushed decode-error notifications as toasts
// in the bottom-right corner — it lives at the root so toasts
// remain visible regardless of which secondary tab DeckRow is
// showing.

import { DeckRow } from "./components/DeckRow";
import { Toaster } from "./components/Toaster";

export const App = (): JSX.Element => {
  return (
    <main aria-label="hypehouse-live root">
      <DeckRow />
      <Toaster />
    </main>
  );
};
