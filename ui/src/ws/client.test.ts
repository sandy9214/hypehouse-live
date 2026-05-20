// Vitest unit tests for JsonRpcWS — mock WebSocket, no real network.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  JsonRpcWS,
  type JsonRpcRequest,
  type WebSocketFactory,
  type WebSocketLike,
} from "./client";

class MockWS implements WebSocketLike {
  public static instances: MockWS[] = [];

  public readyState = 0;
  public sent: string[] = [];
  public onopen: WebSocketLike["onopen"] = null;
  public onclose: WebSocketLike["onclose"] = null;
  public onerror: WebSocketLike["onerror"] = null;
  public onmessage: WebSocketLike["onmessage"] = null;

  public constructor(public url: string) {
    MockWS.instances.push(this);
  }

  public open(): void {
    this.readyState = 1;
    this.onopen?.call(this, new Event("open"));
  }

  public send(data: string): void {
    this.sent.push(data);
  }

  public close(): void {
    this.readyState = 3;
    this.onclose?.call(this, new CloseEvent("close"));
  }

  public emit(payload: unknown): void {
    this.onmessage?.call(
      this,
      new MessageEvent("message", { data: JSON.stringify(payload) }),
    );
  }

  public lastSent(): JsonRpcRequest {
    const raw = this.sent[this.sent.length - 1];
    if (!raw) throw new Error("no messages sent");
    return JSON.parse(raw) as JsonRpcRequest;
  }
}

const makeFactory = (): WebSocketFactory => (url: string) => new MockWS(url);

beforeEach(() => {
  MockWS.instances = [];
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("JsonRpcWS", () => {
  it("sends auth.hello as the first message on open", () => {
    const factory = makeFactory();
    const c = new JsonRpcWS({
      url: "ws://test/ws",
      token: "secret-xyz",
      factory,
    });
    c.connect();
    const sock = MockWS.instances[0]!;
    sock.open();

    expect(sock.sent.length).toBe(1);
    const first = sock.lastSent();
    expect(first.jsonrpc).toBe("2.0");
    expect(first.method).toBe("auth.hello");
    expect(first.id).toBe(1);
    expect(first.params).toEqual({ token: "secret-xyz" });
  });

  it("pairs call() with its response via id and resolves the result", async () => {
    const factory = makeFactory();
    const c = new JsonRpcWS({ url: "ws://test/ws", factory });
    c.connect();
    const sock = MockWS.instances[0]!;
    sock.open();

    const p = c.call<{ ok: true }>("engine.snapshot");
    const req = sock.lastSent();
    expect(req.method).toBe("engine.snapshot");
    expect(req.id).toBe(2); // id=1 was the auth handshake.

    sock.emit({ jsonrpc: "2.0", id: req.id, result: { ok: true } });
    await expect(p).resolves.toEqual({ ok: true });
  });

  it("rejects call() when the server returns a JSON-RPC error", async () => {
    const factory = makeFactory();
    const c = new JsonRpcWS({ url: "ws://test/ws", factory });
    c.connect();
    const sock = MockWS.instances[0]!;
    sock.open();

    const p = c.call("engine.no_such");
    const req = sock.lastSent();
    sock.emit({
      jsonrpc: "2.0",
      id: req.id,
      error: { code: -32601, message: "Method not found" },
    });
    await expect(p).rejects.toThrow(/-32601/);
  });

  it("invokes subscribe handlers for server-pushed notifications", () => {
    const factory = makeFactory();
    const c = new JsonRpcWS({ url: "ws://test/ws", factory });
    c.connect();
    const sock = MockWS.instances[0]!;
    sock.open();

    const handler = vi.fn();
    c.subscribe(handler);

    sock.emit({
      jsonrpc: "2.0",
      method: "engine.state_changed",
      params: { state: { crossfader: 0.42 }, last_event_id: 7 },
    });

    expect(handler).toHaveBeenCalledTimes(1);
    const call = handler.mock.calls[0]![0] as {
      method: string;
      params: { last_event_id: number };
    };
    expect(call.method).toBe("engine.state_changed");
    expect(call.params.last_event_id).toBe(7);
  });

  it("auto-reconnects after the socket closes (1s backoff)", () => {
    const factory = makeFactory();
    const c = new JsonRpcWS({
      url: "ws://test/ws",
      factory,
      initialBackoffMs: 1000,
    });
    c.connect();
    const first = MockWS.instances[0]!;
    first.open();
    expect(MockWS.instances.length).toBe(1);

    // Server-side drop.
    first.close();
    expect(MockWS.instances.length).toBe(1);

    // After the 1s backoff timer fires, a new socket is created.
    vi.advanceTimersByTime(1000);
    expect(MockWS.instances.length).toBe(2);

    // user-initiated close cancels further reconnects.
    const second = MockWS.instances[1]!;
    second.open();
    c.close();
    second.close();
    vi.advanceTimersByTime(60_000);
    expect(MockWS.instances.length).toBe(2);
  });

  it("onOpen subscribers fire on initial connect AND on reconnect", () => {
    const factory = makeFactory();
    const c = new JsonRpcWS({
      url: "ws://test/ws",
      factory,
      initialBackoffMs: 100,
    });
    let callCount = 0;
    const unsub = c.onOpen((): void => {
      callCount += 1;
    });
    c.connect();
    MockWS.instances[0]!.open();
    expect(callCount).toBe(1);

    MockWS.instances[0]!.close();
    vi.advanceTimersByTime(150);
    MockWS.instances[1]!.open();
    expect(callCount).toBe(2);

    unsub();
    MockWS.instances[1]!.close();
    vi.advanceTimersByTime(300);
    MockWS.instances[2]!.open();
    expect(callCount).toBe(2);

    c.close();
  });

  it("pending call() rejects on server-side socket close (Codex #207 R1)", async () => {
    // Regression for the "fetchInFlight latches on after disconnect"
    // bug Codex caught on #207 R1. The store-level in-flight guard
    // (sessionInfo.fetchInFlight) only clears in the `finally` block
    // of the await chain — if handleClose() doesn't reject pending
    // calls, that finally never runs and future reconnects can't
    // refetch.
    const factory = makeFactory();
    const c = new JsonRpcWS({ url: "ws://test/ws", factory });
    c.connect();
    MockWS.instances[0]!.open();
    // Fire a call(), don't wait for response — the test forces a
    // server-side close before any reply lands.
    const inFlight = c.call("engine.session_info");
    MockWS.instances[0]!.close();
    // The promise must reject — otherwise the awaiter hangs forever.
    await expect(inFlight).rejects.toThrow(/connection closed/);
    c.close();
  });

  it("onClose subscribers fire on server-side drop AND on user close", () => {
    const factory = makeFactory();
    const c = new JsonRpcWS({
      url: "ws://test/ws",
      factory,
      initialBackoffMs: 100,
    });
    let closeCount = 0;
    c.onClose((): void => {
      closeCount += 1;
    });
    c.connect();
    MockWS.instances[0]!.open();
    expect(c.isOpen()).toBe(true);
    // Server-side close → onClose fires.
    MockWS.instances[0]!.close();
    expect(closeCount).toBe(1);
    expect(c.isOpen()).toBe(false);
    // Reconnect then user-initiated close → onClose fires again.
    vi.advanceTimersByTime(150);
    MockWS.instances[1]!.open();
    c.close();
    expect(closeCount).toBe(2);
  });

  it("onOpen subscriber exception doesn't disrupt auth flow", () => {
    // Suppress the expected console.error spam from the throwing
    // listener so the test output stays clean.
    const errSpy = vi.spyOn(console, "error").mockImplementation((): void => {});
    const factory = makeFactory();
    const c = new JsonRpcWS({ url: "ws://test/ws", factory });
    c.onOpen((): void => {
      throw new Error("flaky listener");
    });
    let second = 0;
    c.onOpen((): void => {
      second += 1;
    });
    c.connect();
    MockWS.instances[0]!.open();
    expect(second).toBe(1);
    const sent = JSON.parse(MockWS.instances[0]!.sent[0]!);
    expect(sent.method).toBe("auth.hello");
    c.close();
    errSpy.mockRestore();
  });
});
