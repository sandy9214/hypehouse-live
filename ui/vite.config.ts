import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Vite config for hypehouse-live UI.
//
// - port 5173 (Vite default, pinned explicitly so tooling agrees).
// - /ws is proxied (ws=true) to the Rust engine bridge on 127.0.0.1:8765
//   (default address per docs/api/ws-protocol.md). Override via env at
//   deploy time; for dev we want a single-origin URL so the browser's
//   WebSocket constructor and any future cookie/header rewrites stay
//   straightforward.
// - test config wires jsdom + the vitest setup file so component tests
//   get a DOM and our WebSocket mock primitives.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      "/ws": {
        target: "ws://127.0.0.1:8765",
        ws: true,
        changeOrigin: true,
      },
    },
  },
  test: {
    environment: "jsdom",
    globals: false,
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
