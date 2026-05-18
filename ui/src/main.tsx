// React entry point — mounts <App /> into #app.
//
// Opt-in Sentry telemetry boots BEFORE the React tree is rendered so
// the SDK can catch crashes during the very first render pass.
// `initTelemetry` is a no-op unless the operator has explicitly
// enabled telemetry — see `docs/telemetry.md`.

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { initTelemetry } from "./telemetry";

initTelemetry();

const container = document.getElementById("app");
if (!container) {
  throw new Error("#app root element missing from index.html");
}

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
