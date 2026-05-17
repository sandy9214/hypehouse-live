// hypehouse-live UI entry point — v0.1 scaffold.
//
// Subsequent PRs will:
//   1. Open a WebSocket to the Rust engine (default ws://localhost:8765).
//   2. Mount the 2-deck UI: waveforms, EQ knobs, crossfader.
//   3. Wire WebMIDI listener for hardware controllers (DDJ-200 mapping).

const STATUS_EL = document.getElementById("app");

function boot(): void {
  if (!STATUS_EL) return;
  const p = document.createElement("p");
  p.textContent = "engine bridge not yet implemented — UI v0.1 scaffold only";
  STATUS_EL.appendChild(p);
}

boot();
