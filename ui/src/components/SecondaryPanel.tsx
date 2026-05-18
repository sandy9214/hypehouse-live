// SecondaryPanel — Library / Sessions tab chrome + drawer container.
//
// Extracted from DeckRow.tsx so DeckRow stays under the 250-line
// per-component budget. Owns:
//   - The Library / History tab toggle.
//   - The mobile "tap to open / close" drawer affordance.
//   - The drawer wrapper element (`.hh-library-drawer`) whose CSS
//     gets media-query-flipped at < 768 px (see responsive.css.ts).
//
// State (tab + open) is hoisted up to DeckRow so future surfaces
// (e.g. MIDI mapping panel) can swap tabs without crossing through
// this component.

import type { CSSProperties, JSX } from "react";
import { Library } from "./Library";
import { Sessions } from "./Sessions";
import type { JsonRpcWS } from "../ws/client";

export type SecondaryTab = "live" | "history";

export interface SecondaryPanelProps {
  client: JsonRpcWS;
  tab: SecondaryTab;
  setTab: (t: SecondaryTab) => void;
  isMobile: boolean;
  libraryOpen: boolean;
  setLibraryOpen: (open: boolean) => void;
}

const tabRowStyle: CSSProperties = {
  display: "flex",
  gap: 4,
  padding: "4px 8px",
  borderTop: "1px solid #222",
  background: "#080808",
};

const tabButtonStyle = (active: boolean): CSSProperties => ({
  background: active ? "#1c2a3d" : "#0c0c0c",
  color: "#cce0ff",
  border: "1px solid #2c4361",
  borderRadius: 3,
  padding: "3px 12px",
  fontSize: 11,
  fontFamily: "monospace",
  cursor: "pointer",
});

const closeButtonStyle: CSSProperties = {
  ...tabButtonStyle(false),
  marginLeft: "auto",
};

export const SecondaryPanel = ({
  client,
  tab,
  setTab,
  isMobile,
  libraryOpen,
  setLibraryOpen,
}: SecondaryPanelProps): JSX.Element => (
  <>
    <div style={tabRowStyle}>
      <button
        type="button"
        onClick={(): void => {
          setTab("live");
          if (isMobile) setLibraryOpen(!libraryOpen);
        }}
        data-testid="tab-live"
        aria-pressed={tab === "live"}
        aria-expanded={isMobile ? libraryOpen : undefined}
        style={tabButtonStyle(tab === "live")}
      >
        Library
      </button>
      <button
        type="button"
        onClick={(): void => {
          setTab("history");
          if (isMobile) setLibraryOpen(true);
        }}
        data-testid="tab-history"
        aria-pressed={tab === "history"}
        style={tabButtonStyle(tab === "history")}
      >
        History
      </button>
      {isMobile && libraryOpen ? (
        <button
          type="button"
          onClick={(): void => setLibraryOpen(false)}
          data-testid="library-drawer-close"
          aria-label="Close library drawer"
          style={closeButtonStyle}
        >
          Close
        </button>
      ) : null}
    </div>
    <div
      className="hh-library-drawer"
      data-testid="library-drawer"
      data-open={isMobile ? String(libraryOpen) : "true"}
    >
      {tab === "live" ? (
        <Library client={client} />
      ) : (
        <Sessions client={client} />
      )}
    </div>
  </>
);
