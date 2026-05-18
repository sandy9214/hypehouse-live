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
});
