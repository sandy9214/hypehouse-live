// OutputDevicePicker.test.tsx — store + dropdown integration.

// Vitest 4 + jsdom 29 ship a non-spec localStorage (plain object, no
// Storage prototype + missing methods). Polyfill with a Map-backed
// spec-shaped object so the store's guarded reads/writes round-trip.
// Mirrors the trick used in MappingStore.test.ts + Onboarding.test.tsx.
const installLocalStoragePolyfill = (): void => {
  const store = new Map<string, string>();
  const polyfill = {
    getItem: (k: string): string | null =>
      store.has(k) ? (store.get(k) as string) : null,
    setItem: (k: string, v: string): void => {
      store.set(k, String(v));
    },
    removeItem: (k: string): void => {
      store.delete(k);
    },
    clear: (): void => store.clear(),
    key: (i: number): string | null => Array.from(store.keys())[i] ?? null,
    get length(): number {
      return store.size;
    },
  };
  Object.defineProperty(window, "localStorage", {
    configurable: true,
    writable: true,
    value: polyfill,
  });
};
installLocalStoragePolyfill();

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { OutputDevicePicker } from "./OutputDevicePicker";
import {
  __resetOutputDevices,
  __setOutputDevices,
  matchSelectedDevice,
  getSelectedDeviceSubstring,
  setSelectedDeviceSubstring,
  type OutputDeviceList,
} from "../store/outputDevices";
import type { JsonRpcWS } from "../ws/client";

const makeClient = (
  call: ((method: string, params?: unknown) => Promise<unknown>) | null = null,
): JsonRpcWS =>
  ({
    call:
      call ??
      vi.fn().mockResolvedValue({
        devices: [
          { name: "MacBook Pro Speakers", is_default: true },
          { name: "BlackHole 2ch", is_default: false },
        ],
      }),
  }) as unknown as JsonRpcWS;

describe("OutputDevicePicker", () => {
  beforeEach((): void => {
    __resetOutputDevices();
    // Some jsdom builds expose `localStorage` without a working
    // `clear()` — remove our specific key manually so tests stay
    // hermetic across versions.
    if (typeof window !== "undefined") {
      try {
        window.localStorage.removeItem("hypehouse:outputDeviceSubstring");
      } catch {
        // Ignore — localStorage may be sandboxed away.
      }
    }
  });

  afterEach((): void => {
    cleanup();
  });

  it("renders System default option even before fetch resolves", () => {
    render(<OutputDevicePicker client={makeClient()} />);
    const select = screen.getByLabelText(
      "Output audio device",
    ) as HTMLSelectElement;
    expect(select).toBeTruthy();
    expect(select.value).toBe("__default__");
  });

  it("populates dropdown after device list arrives", async () => {
    const list: OutputDeviceList = [
      { name: "Built-in Output", is_default: true },
      { name: "BlackHole 2ch", is_default: false },
      { name: "VB-Cable", is_default: false },
    ];
    __setOutputDevices(list);
    render(<OutputDevicePicker client={makeClient()} />);
    expect(screen.getByText("Built-in Output (default)")).toBeTruthy();
    expect(screen.getByText("BlackHole 2ch")).toBeTruthy();
    expect(screen.getByText("VB-Cable")).toBeTruthy();
  });

  it("persists selection to localStorage on change", () => {
    __setOutputDevices([
      { name: "Built-in Output", is_default: true },
      { name: "BlackHole 2ch", is_default: false },
    ]);
    render(<OutputDevicePicker client={makeClient()} />);
    const select = screen.getByLabelText(
      "Output audio device",
    ) as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "BlackHole 2ch" } });
    expect(getSelectedDeviceSubstring()).toBe("BlackHole 2ch");
  });

  it("clears localStorage when reverting to System default", () => {
    setSelectedDeviceSubstring("BlackHole 2ch");
    __setOutputDevices([
      { name: "Built-in Output", is_default: true },
      { name: "BlackHole 2ch", is_default: false },
    ]);
    render(<OutputDevicePicker client={makeClient()} />);
    const select = screen.getByLabelText(
      "Output audio device",
    ) as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "__default__" } });
    expect(getSelectedDeviceSubstring()).toBe("");
  });

  it("shows the restart hint when a non-default device is selected", () => {
    setSelectedDeviceSubstring("BlackHole 2ch");
    __setOutputDevices([
      { name: "Built-in Output", is_default: true },
      { name: "BlackHole 2ch", is_default: false },
    ]);
    render(<OutputDevicePicker client={makeClient()} />);
    expect(screen.getByTestId("output-device-restart-hint")).toBeTruthy();
  });

  it("hides the restart hint when default is selected", () => {
    __setOutputDevices([
      { name: "Built-in Output", is_default: true },
      { name: "BlackHole 2ch", is_default: false },
    ]);
    render(<OutputDevicePicker client={makeClient()} />);
    expect(screen.queryByTestId("output-device-restart-hint")).toBeNull();
  });

  it("kicks off the engine.list_output_devices RPC on mount", async () => {
    const call = vi.fn().mockResolvedValue({ devices: [] });
    render(<OutputDevicePicker client={makeClient(call)} />);
    await waitFor(() => {
      expect(call).toHaveBeenCalledWith("engine.list_output_devices");
    });
  });
});

describe("matchSelectedDevice", () => {
  it("case-insensitive substring match", () => {
    const list: OutputDeviceList = [
      { name: "Built-in Output", is_default: true },
      { name: "BlackHole 2ch", is_default: false },
    ];
    expect(matchSelectedDevice(list, "blackhole")?.name).toBe("BlackHole 2ch");
    expect(matchSelectedDevice(list, "BUILT")?.name).toBe("Built-in Output");
  });

  it("returns null on no match", () => {
    const list: OutputDeviceList = [{ name: "Built-in Output", is_default: true }];
    expect(matchSelectedDevice(list, "nonexistent")).toBeNull();
  });

  it("treats empty / whitespace substring as no selection", () => {
    const list: OutputDeviceList = [{ name: "X", is_default: true }];
    expect(matchSelectedDevice(list, "")).toBeNull();
    expect(matchSelectedDevice(list, "   ")).toBeNull();
  });
});
