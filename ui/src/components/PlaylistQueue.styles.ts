// PlaylistQueue.styles.ts — extracted style bundle.
//
// Lives in a sibling file so PlaylistQueue.tsx stays under the
// 250-line component budget (the long CSS literal would otherwise
// dominate the file).

import type { CSSProperties } from "react";

export const playlistStyles = {
  container: {
    background: "#0c0c0c",
    borderTop: "1px solid #333",
    color: "#ddd",
    display: "flex",
    flexDirection: "column",
    minHeight: 180,
    maxHeight: 240,
    fontFamily: "monospace",
  },
  header: {
    display: "flex",
    alignItems: "center",
    gap: 8,
    padding: "6px 8px",
    borderBottom: "1px solid #222",
    background: "#101010",
  },
  label: {
    fontSize: 12,
    textTransform: "uppercase",
    letterSpacing: 1,
    opacity: 0.7,
  },
  count: { fontSize: 12, opacity: 0.6 },
  clearBtn: {
    marginLeft: "auto",
    background: "#3a1a1a",
    color: "#f3c8c8",
    border: "1px solid #5a2a2a",
    borderRadius: 4,
    padding: "2px 8px",
    cursor: "pointer",
    fontFamily: "monospace",
    fontSize: 11,
  },
  list: { overflowY: "auto", flex: 1 },
  row: {
    display: "grid",
    gridTemplateColumns: "32px 2fr 64px 56px 96px",
    alignItems: "center",
    gap: 8,
    padding: "4px 8px",
    borderBottom: "1px solid #222",
    fontSize: 12,
  },
  cell: {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
  },
  pos: {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    textAlign: "right",
    opacity: 0.6,
  },
  btn: {
    background: "#222",
    color: "#fff",
    border: "1px solid #444",
    borderRadius: 4,
    padding: "2px 6px",
    cursor: "pointer",
    fontFamily: "monospace",
    fontSize: 11,
  },
  actions: {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    display: "flex",
    gap: 4,
  },
  empty: {
    padding: 16,
    textAlign: "center",
    opacity: 0.7,
    fontSize: 13,
    lineHeight: 1.5,
  },
  error: {
    padding: "4px 8px",
    background: "#3a1a1a",
    color: "#f3c8c8",
    fontSize: 12,
  },
  drop: { outline: "1px dashed #4a8", outlineOffset: -2 },
  missing: {
    fontSize: 10,
    background: "#5a2a2a",
    color: "#f3c8c8",
    padding: "1px 4px",
    borderRadius: 3,
    marginLeft: 4,
  },
} as const satisfies Record<string, CSSProperties>;
