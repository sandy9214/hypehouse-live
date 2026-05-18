// StemRack.test.tsx — render + interaction assertions for the
// per-deck 4-stem mute rack.
//
// Covered cases (≥ 6 — see ADR-002 testing guidelines):
//   1. all 4 mute buttons render with the correct labels (V/D/B/O)
//   2. clicking the vocals button in stem-mode emits
//      `SetStemGain{deck, stem:0, gain:0}`
//   3. clicking a muted button (gain=0) re-emits gain=1 (toggle round-trip)
//   4. buttons disable when stem_mode=false + carry the "Load stems first"
//      tooltip
//   5. clicks in full-mix mode do NOT emit any RPC
//   6. each button's accent colour matches the canonical demucs palette
//      (data-stem-color attribute keeps the check style-agnostic)
//   7. aria-pressed mirrors the audible/muted state (a11y contract)

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { StemRack } from "./StemRack";
import type { JsonRpcWS } from "../ws/client";

type Call = ReturnType<typeof vi.fn>;
const makeClient = (): { client: JsonRpcWS; call: Call } => {
  const call = vi.fn().mockResolvedValue(undefined);
  return { client: { call } as unknown as JsonRpcWS, call };
};

// Capture just the SetStemGain payloads — keeps assertions readable.
const stemEvents = (call: Call): Array<{
  deck: string;
  stem: number;
  gain: number;
}> =>
  call.mock.calls
    .filter((c): boolean => c[0] === "submit_event")
    .map((c): { deck: string; stem: number; gain: number } => {
      const p = c[1] as { SetStemGain?: { deck: string; stem: number; gain: number } };
      return p.SetStemGain!;
    })
    .filter((p): boolean => p !== undefined);

describe("StemRack", () => {
  afterEach((): void => {
    cleanup();
  });

  it("renders 4 mute buttons labelled V / D / B / O", (): void => {
    const { client } = makeClient();
    render(
      <StemRack
        deck="A"
        stemGains={[1, 1, 1, 1]}
        stemMode={true}
        client={client}
      />,
    );
    expect(screen.getByTestId("stem-A-vocals").textContent).toBe("V");
    expect(screen.getByTestId("stem-A-drums").textContent).toBe("D");
    expect(screen.getByTestId("stem-A-bass").textContent).toBe("B");
    expect(screen.getByTestId("stem-A-other").textContent).toBe("O");
  });

  it("clicking vocals in stem-mode emits SetStemGain{stem:0, gain:0}", (): void => {
    const { client, call } = makeClient();
    render(
      <StemRack
        deck="A"
        stemGains={[1, 1, 1, 1]}
        stemMode={true}
        client={client}
      />,
    );
    fireEvent.click(screen.getByTestId("stem-A-vocals"));
    expect(stemEvents(call)).toEqual([{ deck: "A", stem: 0, gain: 0 }]);
  });

  it("clicking a muted stem (gain=0) re-emits gain=1", (): void => {
    const { client, call } = makeClient();
    render(
      <StemRack
        deck="B"
        stemGains={[1, 0, 1, 1]} // drums muted
        stemMode={true}
        client={client}
      />,
    );
    fireEvent.click(screen.getByTestId("stem-B-drums"));
    expect(stemEvents(call)).toEqual([{ deck: "B", stem: 1, gain: 1 }]);
  });

  it("disables all buttons + shows 'Load stems first' tooltip in full-mix mode", (): void => {
    const { client } = makeClient();
    render(
      <StemRack
        deck="A"
        stemGains={[1, 1, 1, 1]}
        stemMode={false}
        client={client}
      />,
    );
    for (const stem of ["vocals", "drums", "bass", "other"] as const) {
      const btn = screen.getByTestId(`stem-A-${stem}`) as HTMLButtonElement;
      expect(btn.disabled).toBe(true);
      expect(btn.getAttribute("title")).toBe("Load stems first");
    }
  });

  it("clicks in full-mix mode emit no SetStemGain RPC", (): void => {
    const { client, call } = makeClient();
    render(
      <StemRack
        deck="A"
        stemGains={[1, 1, 1, 1]}
        stemMode={false}
        client={client}
      />,
    );
    fireEvent.click(screen.getByTestId("stem-A-vocals"));
    fireEvent.click(screen.getByTestId("stem-A-bass"));
    expect(stemEvents(call)).toEqual([]);
  });

  it("each button carries its canonical demucs accent colour", (): void => {
    const { client } = makeClient();
    render(
      <StemRack
        deck="A"
        stemGains={[1, 1, 1, 1]}
        stemMode={true}
        client={client}
      />,
    );
    // Pink / red / purple / cyan — wire to the
    // `data-stem-color` attribute so the test is style-agnostic
    // (re-skinning the CSS doesn't break the contract).
    expect(
      screen.getByTestId("stem-A-vocals").getAttribute("data-stem-color"),
    ).toBe("#ff5fa2");
    expect(
      screen.getByTestId("stem-A-drums").getAttribute("data-stem-color"),
    ).toBe("#ff4d4d");
    expect(
      screen.getByTestId("stem-A-bass").getAttribute("data-stem-color"),
    ).toBe("#a259ff");
    expect(
      screen.getByTestId("stem-A-other").getAttribute("data-stem-color"),
    ).toBe("#00d1c1");
  });

  it("aria-pressed mirrors audible/muted state", (): void => {
    const { client } = makeClient();
    render(
      <StemRack
        deck="A"
        stemGains={[1, 0, 1, 0]} // drums + other muted
        stemMode={true}
        client={client}
      />,
    );
    expect(
      screen.getByTestId("stem-A-vocals").getAttribute("aria-pressed"),
    ).toBe("true");
    expect(
      screen.getByTestId("stem-A-drums").getAttribute("aria-pressed"),
    ).toBe("false");
    expect(
      screen.getByTestId("stem-A-bass").getAttribute("aria-pressed"),
    ).toBe("true");
    expect(
      screen.getByTestId("stem-A-other").getAttribute("aria-pressed"),
    ).toBe("false");
  });

  it("clicking each stem fires the correct stem index", (): void => {
    const { client, call } = makeClient();
    render(
      <StemRack
        deck="A"
        stemGains={[1, 1, 1, 1]}
        stemMode={true}
        client={client}
      />,
    );
    fireEvent.click(screen.getByTestId("stem-A-vocals"));
    fireEvent.click(screen.getByTestId("stem-A-drums"));
    fireEvent.click(screen.getByTestId("stem-A-bass"));
    fireEvent.click(screen.getByTestId("stem-A-other"));
    expect(stemEvents(call).map((e): number => e.stem)).toEqual([0, 1, 2, 3]);
  });
});
