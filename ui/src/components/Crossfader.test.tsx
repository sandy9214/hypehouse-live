// Crossfader.test.tsx — slider must emit an externally-tagged
// `EventKind::Crossfader` frame matching the Rust engine's serde
// default. Wrong shape ({ kind: "CrossfaderSet", value } or
// { CrossfaderSet: value }) would silently fail engine deserialization
// — these tests guard against the regression caught in PR #54.

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Crossfader } from "./Crossfader";

/** Minimal stub of the JSON-RPC client; we only inspect `call` frames. */
class FakeRpc {
  public readonly calls: Array<{ method: string; params: unknown }> = [];
  public call(method: string, params?: unknown): Promise<unknown> {
    this.calls.push({ method, params });
    return Promise.resolve(null);
  }
}

describe("Crossfader", () => {
  afterEach((): void => {
    cleanup();
  });

  it("slider drag emits externally-tagged { Crossfader: { value } } frame", (): void => {
    const client = new FakeRpc();
    render(<Crossfader client={client as unknown as never} value={0.5} />);

    // Slider native range is 0..1000 (1000x granularity over the 0..1
    // engine value). Setting 750 → engine value 0.75.
    const slider = screen.getByTestId("crossfader-input");
    fireEvent.change(slider, { target: { value: "750" } });

    expect(client.calls).toHaveLength(1);
    expect(client.calls[0]!.method).toBe("submit_event");
    expect(client.calls[0]!.params).toEqual({ Crossfader: { value: 0.75 } });
  });

  it("wire shape matches engine serde format (externally-tagged enum)", (): void => {
    // Engine: `pub enum EventKind { Crossfader { value: f32 }, ... }`
    // with default serde derive → externally tagged JSON:
    //   { "Crossfader": { "value": <f32> } }
    // This test pins the JSON snapshot so any drift from the canonical
    // shape (e.g. reverting to { kind, value } or { CrossfaderSet: ... })
    // fails loudly.
    const client = new FakeRpc();
    render(<Crossfader client={client as unknown as never} value={0.5} />);
    const slider = screen.getByTestId("crossfader-input");
    fireEvent.change(slider, { target: { value: "250" } }); // → 0.25

    const frame = client.calls[0]!.params as Record<string, unknown>;
    const keys = Object.keys(frame);
    expect(keys).toEqual(["Crossfader"]); // single externally-tagged variant key
    expect(frame).not.toHaveProperty("kind"); // no internally-tagged stray
    expect(frame).not.toHaveProperty("CrossfaderSet"); // not the wrong variant name
    const payload = frame.Crossfader as Record<string, unknown>;
    expect(Object.keys(payload).sort()).toEqual(["value"]);
    expect(typeof payload.value).toBe("number");
    // Serialised form is the literal string the Rust deserializer expects.
    expect(JSON.stringify(frame)).toBe('{"Crossfader":{"value":0.25}}');
  });

  it("curve dropdown emits externally-tagged { SetCrossfaderCurve: { curve } } frame", (): void => {
    const client = new FakeRpc();
    render(
      <Crossfader
        client={client as unknown as never}
        value={0.5}
        curve="Linear"
      />,
    );
    const dropdown = screen.getByTestId("crossfader-curve-select");
    fireEvent.change(dropdown, { target: { value: "Dipped" } });
    expect(client.calls).toHaveLength(1);
    expect(client.calls[0]!.method).toBe("submit_event");
    expect(client.calls[0]!.params).toEqual({
      SetCrossfaderCurve: { curve: "Dipped" },
    });
    // Snapshot-pin the wire string so any drift from the engine's
    // serde external-tag default trips loudly.
    expect(JSON.stringify(client.calls[0]!.params)).toBe(
      '{"SetCrossfaderCurve":{"curve":"Dipped"}}',
    );
  });

  it("curve dropdown emits each of the four engine curve variants", (): void => {
    const client = new FakeRpc();
    render(
      <Crossfader
        client={client as unknown as never}
        value={0.5}
        curve="Linear"
      />,
    );
    const dropdown = screen.getByTestId("crossfader-curve-select");
    for (const curve of ["Linear", "Dipped", "Sharp", "Scratch"]) {
      fireEvent.change(dropdown, { target: { value: curve } });
    }
    expect(client.calls).toHaveLength(4);
    const curves = client.calls.map(
      (c): string =>
        (c.params as { SetCrossfaderCurve: { curve: string } }).SetCrossfaderCurve
          .curve,
    );
    expect(curves).toEqual(["Linear", "Dipped", "Sharp", "Scratch"]);
  });

  it("curve dropdown reflects the active curve prop", (): void => {
    const client = new FakeRpc();
    render(
      <Crossfader
        client={client as unknown as never}
        value={0.5}
        curve="Sharp"
      />,
    );
    const dropdown = screen.getByTestId(
      "crossfader-curve-select",
    ) as HTMLSelectElement;
    expect(dropdown.value).toBe("Sharp");
    // The matching icon variant is rendered alongside.
    expect(screen.getByTestId("crossfader-curve-icon-sharp")).toBeTruthy();
  });

  it("curve dropdown ignores an invalid value (defensive)", (): void => {
    // A direct DOM mutation injecting a value outside the enum should
    // not produce a wire frame — the engine's reducer would no-op the
    // unknown variant, but skipping the call saves a round-trip and
    // protects against an out-of-tree custom UI variant.
    const client = new FakeRpc();
    render(
      <Crossfader
        client={client as unknown as never}
        value={0.5}
        curve="Linear"
      />,
    );
    const dropdown = screen.getByTestId(
      "crossfader-curve-select",
    ) as HTMLSelectElement;
    // Use Object.defineProperty so the select's controlled value isn't
    // overwritten by React before the change handler reads it.
    fireEvent.change(dropdown, { target: { value: "NotARealCurve" } });
    expect(client.calls).toHaveLength(0);
  });
});
