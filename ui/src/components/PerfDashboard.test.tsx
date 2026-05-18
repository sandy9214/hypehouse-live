// PerfDashboard.test.tsx — covers CPU color bands, expand toggle,
// canvas render, and rolling-history trim at 60s.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { PerfDashboard, cpuColorBand } from "./PerfDashboard";
import {
  __resetPerf,
  __resetPerfNowForTest,
  __setPerfNowForTest,
  applyPerfNotification,
  PERF_HISTORY_WINDOW_MS,
} from "../store/perf";

const emit = (perf: Record<string, unknown>): void => {
  applyPerfNotification({
    jsonrpc: "2.0",
    method: "engine.state_changed",
    params: { perf },
  });
};

describe("PerfDashboard", () => {
  beforeEach((): void => {
    __resetPerf();
    __resetPerfNowForTest();
  });
  afterEach((): void => {
    cleanup();
    __resetPerf();
    __resetPerfNowForTest();
  });

  it("selects green band when CPU% under 50", (): void => {
    expect(cpuColorBand(0)).toBe("green");
    expect(cpuColorBand(49.9)).toBe("green");
  });

  it("selects yellow band in 50..80%", (): void => {
    expect(cpuColorBand(50)).toBe("yellow");
    expect(cpuColorBand(79.9)).toBe("yellow");
  });

  it("selects red band at or above 80%", (): void => {
    expect(cpuColorBand(80)).toBe("red");
    expect(cpuColorBand(200)).toBe("red");
  });

  it("renders the gauge in red when CPU% is hot", (): void => {
    act((): void => {
      emit({
        cpu_percent: 92,
        render_p99_us: 5_000,
        avg_render_us: 4_000,
        callback_period_us: 10_667,
      });
    });
    render(<PerfDashboard />);
    const dash = screen.getByTestId("perf-dashboard");
    expect(dash.getAttribute("data-cpu-band")).toBe("red");
    const gauge = screen.getByTestId("perf-cpu-gauge");
    expect(gauge.getAttribute("data-cpu-band")).toBe("red");
    expect(screen.getByTestId("perf-cpu-readout").textContent).toBe("92.0%");
  });

  it("badges underrun count in red when any xrun has occurred", (): void => {
    act((): void => {
      emit({ underrun_count: 2, decode_underruns: 3 });
    });
    render(<PerfDashboard />);
    const badge = screen.getByTestId("perf-underrun-badge");
    expect(badge.textContent).toContain("5 xruns");
  });

  it("badges 'no xruns' when underrun counters are zero", (): void => {
    render(<PerfDashboard />);
    const badge = screen.getByTestId("perf-underrun-badge");
    expect(badge.textContent).toBe("no xruns");
  });

  it("toggles expanded view on click and renders the chart canvas", (): void => {
    act((): void => {
      emit({ cpu_percent: 25 });
    });
    render(<PerfDashboard />);
    const dash = screen.getByTestId("perf-dashboard");
    expect(dash.getAttribute("data-expanded")).toBe("false");
    fireEvent.click(dash);
    expect(dash.getAttribute("data-expanded")).toBe("true");
    expect(screen.getByTestId("perf-dashboard-expanded")).toBeTruthy();
    expect(screen.getByTestId("perf-chart-canvas")).toBeTruthy();
    fireEvent.click(dash);
    expect(dash.getAttribute("data-expanded")).toBe("false");
  });

  it("history rolls off after 60s, leaving only recent samples", (): void => {
    let t = 1_000_000;
    __setPerfNowForTest((): number => t);
    act((): void => {
      emit({ cpu_percent: 10 });
      t += 1_000;
      emit({ cpu_percent: 20 });
      // Advance past the window — older points must drop.
      t += PERF_HISTORY_WINDOW_MS + 5_000;
      emit({ cpu_percent: 30 });
    });
    render(<PerfDashboard />);
    fireEvent.click(screen.getByTestId("perf-dashboard"));
    // The expanded panel renders a "samples N" caption; only the most
    // recent point should remain after the trim.
    const expanded = screen.getByTestId("perf-dashboard-expanded");
    expect(expanded.textContent).toContain("samples 1");
  });

  it("reports recorder dropped frames when non-zero", (): void => {
    act((): void => {
      emit({ dropped_frames: 47 });
    });
    render(<PerfDashboard />);
    const dropped = screen.getByTestId("perf-dropped-readout");
    expect(dropped.textContent).toBe("47 dropped");
  });

  it("renders 'rec ok' when no dropped frames", (): void => {
    render(<PerfDashboard />);
    const dropped = screen.getByTestId("perf-dropped-readout");
    expect(dropped.textContent).toBe("rec ok");
  });
});
