// Onboarding.styles.ts — pulled out of `Onboarding.tsx` so the component
// itself stays under the 250-line ceiling. All values are pure CSS-in-JS
// (no runtime deps), keyed by role rather than by step number so the
// wizard's body can branch on step without re-shuffling style state.

import type { CSSProperties } from "react";

export const overlayStyle: CSSProperties = {
  position: "fixed",
  inset: 0,
  background: "rgba(0,0,0,0.75)",
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  zIndex: 1000,
  fontFamily: "monospace",
};

export const cardStyle: CSSProperties = {
  background: "#141414",
  color: "#eaeaea",
  border: "1px solid #2a2a2a",
  borderRadius: 8,
  width: "100%",
  maxWidth: 600,
  padding: 24,
  boxShadow: "0 10px 40px rgba(0,0,0,0.6)",
};

export const headerStyle: CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  alignItems: "center",
  marginBottom: 16,
};

export const dotsStyle: CSSProperties = { display: "flex", gap: 8 };

export const dotStyle = (active: boolean): CSSProperties => ({
  width: 10,
  height: 10,
  borderRadius: "50%",
  background: active ? "#4a90e2" : "#333",
  transition: "background 200ms ease",
});

export const closeBtnStyle: CSSProperties = {
  background: "transparent",
  color: "#888",
  border: "none",
  fontSize: 18,
  cursor: "pointer",
  padding: 4,
};

export const bodyStyle: CSSProperties = {
  minHeight: 180,
  fontSize: 13,
  lineHeight: 1.6,
};

export const inputStyle: CSSProperties = {
  width: "100%",
  padding: 8,
  background: "#0a0a0a",
  color: "#fff",
  border: "1px solid #333",
  borderRadius: 4,
  fontFamily: "monospace",
  fontSize: 13,
  marginTop: 8,
};

export const footerStyle: CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  alignItems: "center",
  marginTop: 20,
  gap: 8,
};

export const primaryBtnStyle = (disabled: boolean): CSSProperties => ({
  background: disabled ? "#1a3050" : "#2c5fa3",
  color: disabled ? "#666" : "#fff",
  border: "1px solid #3a6cb0",
  borderRadius: 4,
  padding: "8px 16px",
  fontFamily: "monospace",
  fontSize: 13,
  cursor: disabled ? "not-allowed" : "pointer",
});

export const secondaryBtnStyle: CSSProperties = {
  background: "transparent",
  color: "#aaa",
  border: "1px solid #444",
  borderRadius: 4,
  padding: "8px 16px",
  fontFamily: "monospace",
  fontSize: 13,
  cursor: "pointer",
};

export const linkStyle: CSSProperties = {
  background: "transparent",
  border: "none",
  color: "#7aa9d8",
  textDecoration: "underline",
  cursor: "pointer",
  fontSize: 12,
  fontFamily: "monospace",
};

export const progressBarOuter: CSSProperties = {
  width: "100%",
  height: 8,
  background: "#222",
  borderRadius: 4,
  overflow: "hidden",
  marginTop: 16,
};

export const progressBarInner = (pct: number): CSSProperties => ({
  height: "100%",
  width: `${Math.min(100, Math.max(0, pct))}%`,
  background: "linear-gradient(90deg,#4a90e2,#7aa9d8)",
  transition: "width 400ms ease",
});

export const errorTextStyle: CSSProperties = { color: "#f3c8c8" };

export const sectionHeadingStyle: CSSProperties = {
  margin: "0 0 12px",
  fontSize: 18,
};

export const inputLabelStyle: CSSProperties = { fontSize: 12, opacity: 0.7 };
