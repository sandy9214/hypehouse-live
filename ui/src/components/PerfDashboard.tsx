// PerfDashboard — audio-thread health widget. Lives in the master
// strip next to MasterControls. Surfaces four signals from every
// `engine.state_changed`: CPU% (green<50% / yellow<80% / red≥80%),
// render p99 (µs), cumulative underruns (cpal + decoder), and recorder
// dropped-frame count. Click to expand: 60s rolling-history chart on
// canvas with no external chart lib. See store/perf.ts for the wire.

import { useEffect, useRef, useState } from "react";
import type { CSSProperties } from "react";
import {
  PERF_HISTORY_WINDOW_MS,
  usePerf,
  usePerfHistory,
} from "../store/perf";

const CPU_GREEN_MAX = 50;
const CPU_YELLOW_MAX = 80;

const wrap: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: 4,
  padding: "8px 12px",
  background: "#0e0e0e",
  color: "#ddd",
  fontFamily: "monospace",
  fontSize: 11,
  borderTop: "1px solid #222",
  cursor: "pointer",
  userSelect: "none",
};

const collapsedRow: CSSProperties = {
  display: "flex",
  alignItems: "center",
  gap: 12,
};

const gaugeTrack: CSSProperties = {
  width: 64,
  height: 8,
  background: "#161616",
  border: "1px solid #333",
  borderRadius: 2,
  overflow: "hidden",
  position: "relative",
};

const colorForCpu = (cpu: number): string =>
  cpu < CPU_GREEN_MAX ? "#34d058" : cpu < CPU_YELLOW_MAX ? "#ffd84a" : "#ff3333";

const gaugeFill = (cpu: number): CSSProperties => ({
  position: "absolute",
  left: 0,
  top: 0,
  bottom: 0,
  width: `${Math.max(0, Math.min(100, cpu))}%`,
  background: colorForCpu(cpu),
});

const badge = (count: number): CSSProperties => ({
  padding: "1px 6px",
  borderRadius: 8,
  background: count > 0 ? "#7a1212" : "#1a1a1a",
  color: count > 0 ? "#ffd0d0" : "#666",
  border: `1px solid ${count > 0 ? "#ff3333" : "#333"}`,
  fontSize: 10,
});

const chartCanvas: CSSProperties = {
  width: "100%",
  height: 80,
  background: "#0a0a0a",
  border: "1px solid #222",
  borderRadius: 2,
};

/** Resolve CPU color for tests / dashboard text. Exported so test code
 *  can pin the gauge color logic without snapshot-matching CSS. */
export const cpuColorBand = (cpu: number): "green" | "yellow" | "red" => {
  if (cpu < CPU_GREEN_MAX) return "green";
  if (cpu < CPU_YELLOW_MAX) return "yellow";
  return "red";
};

interface PerfDashboardProps {
  /** Override "now" so tests can pin the chart's time-axis. */
  nowFn?: () => number;
}

export const PerfDashboard = ({
  nowFn,
}: PerfDashboardProps = {}): JSX.Element => {
  const perf = usePerf();
  const history = usePerfHistory();
  const [expanded, setExpanded] = useState(false);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  // Pre-compute color band once so the gauge style + readout match.
  const band = cpuColorBand(perf.cpu_percent);
  const cpuLabel = perf.cpu_percent.toFixed(1);
  const latencyUs = perf.render_p99_us;
  const totalUnderruns = perf.underrun_count + perf.decode_underruns;

  useEffect((): void => {
    if (!expanded) return;
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const dpr =
      typeof window !== "undefined" && window.devicePixelRatio
        ? window.devicePixelRatio
        : 1;
    const w = canvas.clientWidth || 200;
    const h = canvas.clientHeight || 80;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    // Background grid: 50 / 80% horizontal CPU markers.
    ctx.strokeStyle = "#222";
    ctx.lineWidth = 1;
    for (const pct of [50, 80]) {
      const y = h - (pct / 100) * h;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }
    if (history.length < 2) return;
    const now = (nowFn ?? Date.now)();
    const t0 = now - PERF_HISTORY_WINDOW_MS;
    const xFor = (ts: number): number =>
      ((ts - t0) / PERF_HISTORY_WINDOW_MS) * w;
    // Render-p99 line scales against callback_period_us so 100% budget
    // reaches the chart top. Fallback to 10ms when the period isn't
    // yet wired in.
    const periodUs = perf.callback_period_us > 0 ? perf.callback_period_us : 10_000;
    const draw = (
      color: string,
      width: number,
      pick: (p: (typeof history)[number]) => number,
      normalise: (v: number) => number,
    ): void => {
      ctx.strokeStyle = color;
      ctx.lineWidth = width;
      ctx.beginPath();
      let first = true;
      for (const p of history) {
        const x = xFor(p.ts);
        const y = h - normalise(pick(p)) * h;
        if (first) {
          ctx.moveTo(x, y);
          first = false;
        } else {
          ctx.lineTo(x, y);
        }
      }
      ctx.stroke();
    };
    draw("#34d058", 1.5, (p): number => p.cpuPercent, (v): number =>
      Math.min(1, Math.max(0, v / 100)),
    );
    draw("#ffd84a", 1, (p): number => p.renderP99Us, (v): number =>
      Math.min(1, v / periodUs),
    );
  }, [expanded, history, perf.callback_period_us, nowFn]);

  return (
    <div
      style={wrap}
      data-testid="perf-dashboard"
      data-cpu-band={band}
      data-expanded={expanded}
      role="button"
      tabIndex={0}
      onClick={(): void => setExpanded((v): boolean => !v)}
      onKeyDown={(e): void => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          setExpanded((v): boolean => !v);
        }
      }}
      aria-label="audio thread performance dashboard"
      aria-expanded={expanded}
    >
      <div style={collapsedRow}>
        <span aria-hidden="true">CPU</span>
        <div
          style={gaugeTrack}
          data-testid="perf-cpu-gauge"
          data-cpu-band={band}
          role="meter"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={perf.cpu_percent}
          aria-label="audio thread cpu percent"
          title={`avg render ${perf.avg_render_us} µs / period ${perf.callback_period_us} µs`}
        >
          <div style={gaugeFill(perf.cpu_percent)} data-testid="perf-cpu-fill" />
        </div>
        <span data-testid="perf-cpu-readout" style={{ width: 48 }}>
          {`${cpuLabel}%`}
        </span>
        <span data-testid="perf-latency-readout" style={{ width: 80 }}>
          {`p99 ${latencyUs} µs`}
        </span>
        <span data-testid="perf-underrun-badge" style={badge(totalUnderruns)}>
          {totalUnderruns > 0
            ? `${totalUnderruns} xrun${totalUnderruns === 1 ? "" : "s"}`
            : "no xruns"}
        </span>
        <span
          data-testid="perf-dropped-readout"
          style={{ color: perf.dropped_frames > 0 ? "#ffaaaa" : "#666" }}
        >
          {perf.dropped_frames > 0
            ? `${perf.dropped_frames} dropped`
            : "rec ok"}
        </span>
      </div>
      {expanded ? (
        <div data-testid="perf-dashboard-expanded">
          <canvas
            ref={canvasRef}
            style={chartCanvas}
            data-testid="perf-chart-canvas"
            aria-label="audio thread cpu and render latency history"
            role="img"
          />
          <div style={{ display: "flex", gap: 12, fontSize: 10, color: "#888" }}>
            <span style={{ color: "#34d058" }}>CPU%</span>
            <span style={{ color: "#ffd84a" }}>render µs (vs period)</span>
            <span>window 60s · samples {history.length}</span>
          </div>
        </div>
      ) : null}
    </div>
  );
};
