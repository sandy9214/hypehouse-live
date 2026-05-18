// Onboarding.test.tsx — coverage for the first-launch wizard.
//
// Mock-WS client follows the same pattern as `Library.test.tsx`: a
// `vi.fn` keyed off `method` returns canned responses. localStorage is
// reset between cases so the "Skip / Cancel does not set the flag"
// assertion is meaningful.

import { act } from "react";
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { Onboarding } from "./Onboarding";
import type { JsonRpcWS } from "../ws/client";
import {
  ONBOARDING_FLAG_KEY,
  clearOnboardingFlag,
  readOnboardingFlag,
} from "../store/onboarding";

// Vitest 4 + jsdom 29 ships a non-spec localStorage stub (plain object,
// no Storage prototype). Patch one in for the suite so our store's
// guarded reads/writes can round-trip.
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

type Call = (method: string, params?: unknown) => Promise<unknown>;

interface PendingRpc<T> {
  promise: Promise<T>;
  resolve: (v: T) => void;
  reject: (e: Error) => void;
}

const deferred = <T,>(): PendingRpc<T> => {
  let resolve!: (v: T) => void;
  let reject!: (e: Error) => void;
  const promise = new Promise<T>(
    (res: (v: T) => void, rej: (e: Error) => void): void => {
      resolve = res;
      reject = rej;
    },
  );
  return { promise, resolve, reject };
};

const makeClient = (
  responder: (method: string, params?: unknown) => Promise<unknown>,
): { client: JsonRpcWS; call: ReturnType<typeof vi.fn> } => {
  const call = vi.fn<Call>(responder);
  return { client: { call } as unknown as JsonRpcWS, call };
};

describe("Onboarding", () => {
  beforeEach((): void => {
    clearOnboardingFlag();
  });
  afterEach((): void => {
    cleanup();
    clearOnboardingFlag();
    vi.useRealTimers();
  });

  it("renders step 1 (welcome) on mount when flag is missing", (): void => {
    const { client } = makeClient((): Promise<unknown> => Promise.resolve({}));
    render(
      <Onboarding client={client} onClose={(): void => undefined} />,
    );
    expect(screen.getByTestId("onboarding-modal")).toBeTruthy();
    expect(screen.getByTestId("onboarding-step-1")).toBeTruthy();
    expect(screen.getByText(/Welcome to hypehouse-live/)).toBeTruthy();
  });

  it("Next button advances welcome → directory picker", (): void => {
    const { client } = makeClient((): Promise<unknown> => Promise.resolve({}));
    render(
      <Onboarding client={client} onClose={(): void => undefined} />,
    );
    fireEvent.click(screen.getByTestId("onboarding-next"));
    expect(screen.getByTestId("onboarding-step-2")).toBeTruthy();
    expect(screen.getByTestId("onboarding-dir-input")).toBeTruthy();
  });

  it("Scan button is disabled when directory input is empty", (): void => {
    const { client } = makeClient((): Promise<unknown> => Promise.resolve({}));
    render(
      <Onboarding client={client} onClose={(): void => undefined} />,
    );
    fireEvent.click(screen.getByTestId("onboarding-next"));
    const scanBtn = screen.getByTestId("onboarding-scan") as HTMLButtonElement;
    expect(scanBtn.disabled).toBe(true);
    // Whitespace-only still counts as empty.
    fireEvent.change(screen.getByTestId("onboarding-dir-input"), {
      target: { value: "   " },
    });
    expect(scanBtn.disabled).toBe(true);
    fireEvent.change(screen.getByTestId("onboarding-dir-input"), {
      target: { value: "/Users/me/music" },
    });
    expect(scanBtn.disabled).toBe(false);
  });

  it("Scan with valid input invokes library.add_track_from_directory", async (): Promise<void> => {
    const pending = deferred<unknown>();
    const { client, call } = makeClient((method: string): Promise<unknown> => {
      if (method === "library.add_track_from_directory") return pending.promise;
      if (method === "library.list_tracks")
        return Promise.resolve({ tracks: [], total: 0, limit: 1, offset: 0 });
      return Promise.reject(new Error(`unmocked: ${method}`));
    });
    render(
      <Onboarding
        client={client}
        onClose={(): void => undefined}
        pollIntervalMs={0}
      />,
    );
    fireEvent.click(screen.getByTestId("onboarding-next"));
    fireEvent.change(screen.getByTestId("onboarding-dir-input"), {
      target: { value: "/m/library" },
    });
    fireEvent.click(screen.getByTestId("onboarding-scan"));
    await waitFor((): void => {
      const found = call.mock.calls.find(
        (c: unknown[]): boolean =>
          c[0] === "library.add_track_from_directory",
      );
      expect(found).toBeDefined();
      expect(found?.[1]).toEqual({ path: "/m/library" });
    });
    // Cleanup: resolve so the post-RPC state update doesn't leak.
    await act(async (): Promise<void> => {
      pending.resolve({ added: [], added_count: 0, total: 0 });
    });
  });

  it("progress polls library.list_tracks during ingestion", async (): Promise<void> => {
    vi.useFakeTimers();
    const addPending = deferred<unknown>();
    let listCalls = 0;
    const { client, call } = makeClient((method: string): Promise<unknown> => {
      if (method === "library.add_track_from_directory") return addPending.promise;
      if (method === "library.list_tracks") {
        listCalls += 1;
        return Promise.resolve({
          tracks: [],
          total: listCalls * 3,
          limit: 1,
          offset: 0,
        });
      }
      return Promise.reject(new Error(`unmocked: ${method}`));
    });
    render(
      <Onboarding
        client={client}
        onClose={(): void => undefined}
        pollIntervalMs={50}
      />,
    );
    fireEvent.click(screen.getByTestId("onboarding-next"));
    fireEvent.change(screen.getByTestId("onboarding-dir-input"), {
      target: { value: "/m/lib" },
    });
    fireEvent.click(screen.getByTestId("onboarding-scan"));
    // Tick the poll interval twice — two list_tracks calls should fire.
    await act(async (): Promise<void> => {
      await vi.advanceTimersByTimeAsync(120);
    });
    const listInvocations = call.mock.calls.filter(
      (c: unknown[]): boolean => c[0] === "library.list_tracks",
    );
    expect(listInvocations.length).toBeGreaterThanOrEqual(2);
    expect(
      screen.getByTestId("onboarding-progress-count").textContent,
    ).toMatch(/\d+ tracks? analyzed/);
    // Resolve the outer add RPC so cleanup doesn't strand promises.
    await act(async (): Promise<void> => {
      addPending.resolve({ added: [], added_count: 9, total: 9 });
      await vi.advanceTimersByTimeAsync(0);
    });
  });

  it("shows Done state when ingestion completes and writes flag on Start mixing", async (): Promise<void> => {
    const { client } = makeClient((method: string): Promise<unknown> => {
      if (method === "library.add_track_from_directory")
        return Promise.resolve({ added: [], added_count: 12, total: 12 });
      if (method === "library.list_tracks")
        return Promise.resolve({ tracks: [], total: 12, limit: 1, offset: 0 });
      return Promise.reject(new Error(`unmocked: ${method}`));
    });
    let closedWith: boolean | null = null;
    render(
      <Onboarding
        client={client}
        onClose={(c: boolean): void => {
          closedWith = c;
        }}
        pollIntervalMs={0}
      />,
    );
    fireEvent.click(screen.getByTestId("onboarding-next"));
    fireEvent.change(screen.getByTestId("onboarding-dir-input"), {
      target: { value: "/m/lib" },
    });
    fireEvent.click(screen.getByTestId("onboarding-scan"));
    await waitFor((): void => {
      expect(screen.getByTestId("onboarding-done")).toBeTruthy();
    });
    expect(screen.getByTestId("onboarding-done").textContent).toContain("12");
    expect(readOnboardingFlag()).toBe(false);
    fireEvent.click(screen.getByTestId("onboarding-finish"));
    expect(readOnboardingFlag()).toBe(true);
    expect(window.localStorage.getItem(ONBOARDING_FLAG_KEY)).toBe("1");
    expect(closedWith).toBe(true);
  });

  it("Skip leaves the flag unset and signals completed=false", (): void => {
    const { client } = makeClient((): Promise<unknown> => Promise.resolve({}));
    let closedWith: boolean | null = null;
    render(
      <Onboarding
        client={client}
        onClose={(c: boolean): void => {
          closedWith = c;
        }}
      />,
    );
    fireEvent.click(screen.getByTestId("onboarding-skip"));
    expect(readOnboardingFlag()).toBe(false);
    expect(closedWith).toBe(false);
  });

  it("Cancel from step 2 leaves the flag unset (reopens next launch)", (): void => {
    const { client } = makeClient((): Promise<unknown> => Promise.resolve({}));
    let closedWith: boolean | null = null;
    render(
      <Onboarding
        client={client}
        onClose={(c: boolean): void => {
          closedWith = c;
        }}
      />,
    );
    fireEvent.click(screen.getByTestId("onboarding-next"));
    fireEvent.change(screen.getByTestId("onboarding-dir-input"), {
      target: { value: "/m/lib" },
    });
    fireEvent.click(screen.getByTestId("onboarding-cancel"));
    expect(readOnboardingFlag()).toBe(false);
    expect(closedWith).toBe(false);
  });

  it("surfaces an error and offers a Back path when scan RPC fails", async (): Promise<void> => {
    const { client } = makeClient((method: string): Promise<unknown> => {
      if (method === "library.add_track_from_directory")
        return Promise.reject(new Error("scan failed: permission denied"));
      if (method === "library.list_tracks")
        return Promise.resolve({ tracks: [], total: 0, limit: 1, offset: 0 });
      return Promise.reject(new Error(`unmocked: ${method}`));
    });
    render(
      <Onboarding
        client={client}
        onClose={(): void => undefined}
        pollIntervalMs={0}
      />,
    );
    fireEvent.click(screen.getByTestId("onboarding-next"));
    fireEvent.change(screen.getByTestId("onboarding-dir-input"), {
      target: { value: "/m/lib" },
    });
    fireEvent.click(screen.getByTestId("onboarding-scan"));
    await waitFor((): void => {
      expect(screen.getByTestId("onboarding-error")).toBeTruthy();
    });
    expect(
      screen.getByTestId("onboarding-error").textContent,
    ).toContain("permission denied");
    // Back returns to step 2 so the user can fix the path and retry.
    fireEvent.click(screen.getByTestId("onboarding-retry"));
    expect(screen.getByTestId("onboarding-step-2")).toBeTruthy();
  });

  it("step indicator dots reflect the active step", (): void => {
    const { client } = makeClient((): Promise<unknown> => Promise.resolve({}));
    render(
      <Onboarding client={client} onClose={(): void => undefined} />,
    );
    // Step 1: dot 1 is the active blue; dot 3 is still the inactive
    // grey. jsdom serializes hex into `rgb()` form so we compare on
    // the resolved RGB tuple.
    const ACTIVE_RGB = "rgb(74, 144, 226)";
    const INACTIVE_RGB = "rgb(51, 51, 51)";
    expect(
      (screen.getByTestId("dot-1").getAttribute("style") ?? "").toLowerCase(),
    ).toContain(ACTIVE_RGB);
    expect(
      (screen.getByTestId("dot-3").getAttribute("style") ?? "").toLowerCase(),
    ).toContain(INACTIVE_RGB);
    fireEvent.click(screen.getByTestId("onboarding-next"));
    // Step 2: dot 2 now active.
    expect(
      (screen.getByTestId("dot-2").getAttribute("style") ?? "").toLowerCase(),
    ).toContain(ACTIVE_RGB);
  });
});
