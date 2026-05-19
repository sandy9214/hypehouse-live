// Sessions.styles.ts — extracted CSSProperties for `Sessions.tsx` so the
// component itself stays under the 250-line cap. Kept as plain objects
// (no css-in-js runtime) — identical to the inline-style pattern the
// rest of the v0.1 UI uses.

import type { CSSProperties } from "react";

export const containerStyle: CSSProperties = {
  background: "#0c0c0c",
  color: "#ddd",
  display: "flex",
  flexDirection: "column",
  flex: 1,
  minHeight: 0,
  fontFamily: "monospace",
};

export const headerStyle: CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 8,
  padding: "8px 10px",
  borderBottom: "1px solid #222",
  background: "#101010",
  fontSize: 12,
  textTransform: "uppercase",
  letterSpacing: 1,
  opacity: 0.85,
};

export const bodyStyle: CSSProperties = {
  display: "flex",
  flex: 1,
  minHeight: 0,
};

export const listColStyle: CSSProperties = {
  flex: 1,
  minWidth: 0,
  overflowY: "auto",
  borderRight: "1px solid #222",
};

export const detailColStyle: CSSProperties = {
  flex: 1,
  minWidth: 0,
  overflowY: "auto",
  padding: "10px 12px",
  fontSize: 12,
};

// 6 columns now (added "Actions" for the export button).
export const columnsRowStyle: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "1.6fr 1.2fr 0.7fr 0.6fr 0.8fr 1.1fr",
  gap: 8,
  padding: "6px 10px",
  fontSize: 11,
  textTransform: "uppercase",
  opacity: 0.55,
  borderBottom: "1px solid #1c1c1c",
  background: "#0a0a0a",
};

export const rowBaseStyle: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "1.6fr 1.2fr 0.7fr 0.6fr 0.8fr 1.1fr",
  gap: 8,
  padding: "6px 10px",
  fontSize: 12,
  borderBottom: "1px solid #161616",
  cursor: "pointer",
  background: "transparent",
  alignItems: "center",
};

export const rowSelectedStyle: CSSProperties = {
  ...rowBaseStyle,
  background: "#19283a",
};

export const recBadgeStyle: CSSProperties = {
  display: "inline-block",
  padding: "1px 6px",
  borderRadius: 3,
  fontSize: 10,
  background: "#274028",
  color: "#9ee29a",
  border: "1px solid #2f5331",
};

export const recBadgeMissingStyle: CSSProperties = {
  ...recBadgeStyle,
  background: "#1c1c1c",
  color: "#666",
  borderColor: "#2a2a2a",
};

export const buttonStyle: CSSProperties = {
  background: "#1c2a3d",
  color: "#cce0ff",
  border: "1px solid #2c4361",
  borderRadius: 3,
  padding: "3px 8px",
  fontSize: 11,
  cursor: "pointer",
  fontFamily: "monospace",
};

export const emptyStyle: CSSProperties = {
  padding: "16px",
  textAlign: "center",
  opacity: 0.6,
  fontSize: 13,
};

export const errorBannerStyle: CSSProperties = {
  padding: "6px 10px",
  background: "#3a1a1a",
  color: "#ffb0b0",
  borderBottom: "1px solid #5a2727",
  fontSize: 12,
};
