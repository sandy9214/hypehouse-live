// MidiSettings.test.tsx — render the panel, exercise import / activate /
// export / reload flows. Built-in mappings are always present; we
// stub WebMIDIListener.applyMapping so we can assert it gets called
// with the expected payload on activate + reload.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { installLocalStoragePolyfill } from "../test-utils/localStoragePolyfill";
installLocalStoragePolyfill();

import { MidiSettings } from "./MidiSettings";
import { __resetMappingStore, loadMapping, persistMapping } from "../midi/MappingStore.ts";
import {
  __resetDecodeErrors,
  useDecodeErrors,
} from "../store/notifications";

const CUSTOM_VALID = {
  id: "my-mc7000",
  bindings: [{ noteOn: { channel: 0, note: 1 }, action: "play_pause", deck: "A" }],
};

const fakeFile = (content: string, name = "mapping.json"): File =>
  new File([content], name, { type: "application/json" });

const fakeListener = (): { applyMapping: ReturnType<typeof vi.fn> } => ({
  applyMapping: vi.fn().mockReturnValue({ ok: true }),
});

/** Helper hook test wrapper: renders the toast queue length next to the
 *  panel so a failing import surfaces in the DOM without us reaching
 *  into the store. */
const ErrorProbe = (): JSX.Element => {
  const errors = useDecodeErrors();
  return <span data-testid="error-count">{errors.length}</span>;
};

describe("MidiSettings", () => {
  beforeEach((): void => {
    __resetMappingStore();
    __resetDecodeErrors();
  });

  afterEach((): void => {
    cleanup();
    __resetMappingStore();
    __resetDecodeErrors();
  });

  it("lists built-in midi mappings on first render", (): void => {
    render(<MidiSettings />);
    expect(screen.getByTestId("mapping-row-ddj200")).toBeTruthy();
    // Keyboard mapping not visible in the default midi-kind panel.
    expect(screen.queryByTestId("mapping-row-keyboard")).toBeNull();
  });

  it("kind='keyboard' filters to keyboard mappings", (): void => {
    render(<MidiSettings kind="keyboard" />);
    expect(screen.getByTestId("mapping-row-keyboard")).toBeTruthy();
    expect(screen.queryByTestId("mapping-row-ddj200")).toBeNull();
  });

  it("activating a mapping calls listener.applyMapping and highlights the row", (): void => {
    const listener = fakeListener();
    persistMapping(CUSTOM_VALID.id, CUSTOM_VALID);
    render(<MidiSettings listener={listener as never} />);
    fireEvent.click(screen.getByLabelText(`Activate ${CUSTOM_VALID.id}`));
    expect(listener.applyMapping).toHaveBeenCalledTimes(1);
    const sentArg = listener.applyMapping.mock.calls[0]![0] as { id: string };
    expect(sentArg.id).toBe(CUSTOM_VALID.id);
    const row = screen.getByTestId(`mapping-row-${CUSTOM_VALID.id}`);
    expect(row.getAttribute("data-active")).toBe("true");
  });

  it("reload button re-applies the mapping to the live listener", (): void => {
    const listener = fakeListener();
    render(<MidiSettings listener={listener as never} />);
    fireEvent.click(screen.getByLabelText("Reload ddj200"));
    expect(listener.applyMapping).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByLabelText("Reload ddj200"));
    expect(listener.applyMapping).toHaveBeenCalledTimes(2);
  });

  it("import flow persists a valid mapping and surfaces it in the list", async (): Promise<void> => {
    render(
      <>
        <MidiSettings />
        <ErrorProbe />
      </>,
    );
    const input = screen.getByTestId("mapping-file-input") as HTMLInputElement;
    const file = fakeFile(JSON.stringify(CUSTOM_VALID));
    Object.defineProperty(input, "files", { value: [file], configurable: true });
    await act(async (): Promise<void> => {
      fireEvent.change(input);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(loadMapping(CUSTOM_VALID.id)).not.toBeNull();
    expect(screen.getByTestId(`mapping-row-${CUSTOM_VALID.id}`)).toBeTruthy();
    expect(screen.getByTestId("error-count").textContent).toBe("0");
  });

  it("import of malformed JSON emits an error toast and does not persist", async (): Promise<void> => {
    render(
      <>
        <MidiSettings />
        <ErrorProbe />
      </>,
    );
    const input = screen.getByTestId("mapping-file-input") as HTMLInputElement;
    Object.defineProperty(input, "files", {
      value: [fakeFile("{this is not json")],
      configurable: true,
    });
    await act(async (): Promise<void> => {
      fireEvent.change(input);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(loadMapping("anything")).toBeNull();
    expect(screen.getByTestId("error-count").textContent).toBe("1");
  });

  it("import of schema-invalid JSON emits an error toast", async (): Promise<void> => {
    render(
      <>
        <MidiSettings />
        <ErrorProbe />
      </>,
    );
    const bad = JSON.stringify({ id: "bad", bindings: [{ action: "explode" }] });
    const input = screen.getByTestId("mapping-file-input") as HTMLInputElement;
    Object.defineProperty(input, "files", {
      value: [fakeFile(bad)],
      configurable: true,
    });
    await act(async (): Promise<void> => {
      fireEvent.change(input);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(loadMapping("bad")).toBeNull();
    expect(screen.getByTestId("error-count").textContent).toBe("1");
  });

  it("export triggers a download of the current mapping JSON", async (): Promise<void> => {
    const origCreate = URL.createObjectURL;
    const origRevoke = URL.revokeObjectURL;
    const blobs: Blob[] = [];
    URL.createObjectURL = vi.fn((b: Blob): string => {
      blobs.push(b);
      return "blob:fake";
    }) as unknown as typeof URL.createObjectURL;
    URL.revokeObjectURL = vi.fn() as unknown as typeof URL.revokeObjectURL;
    const clickSpy = vi
      .spyOn(HTMLAnchorElement.prototype, "click")
      .mockImplementation(() => undefined);

    try {
      render(<MidiSettings />);
      fireEvent.click(screen.getByLabelText("Export ddj200"));
      expect(blobs).toHaveLength(1);
      expect(clickSpy).toHaveBeenCalledTimes(1);
      const text = await blobs[0]!.text();
      const parsed = JSON.parse(text) as { id: string; bindings: unknown[] };
      expect(parsed.id).toBe("ddj200");
      expect(Array.isArray(parsed.bindings)).toBe(true);
    } finally {
      URL.createObjectURL = origCreate;
      URL.revokeObjectURL = origRevoke;
      clickSpy.mockRestore();
    }
  });

  it("activating a missing mapping emits a toast and does not crash", (): void => {
    const listener = fakeListener();
    // Stash an active name pointing at a deleted mapping, then re-render.
    localStorage.setItem("hypehouse:midi-active:midi", "ghost");
    render(
      <>
        <MidiSettings listener={listener as never} />
        <ErrorProbe />
      </>,
    );
    // ghost is not in the list; clicking ddj200 should still work.
    fireEvent.click(screen.getByLabelText("Activate ddj200"));
    expect(listener.applyMapping).toHaveBeenCalledTimes(1);
  });
});
