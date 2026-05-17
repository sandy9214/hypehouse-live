// JSON-RPC 2.0 WebSocket client for the hypehouse-live Rust engine.
//
// See docs/api/ws-protocol.md (ADR-001 stack choice). Per task spec,
// authentication is performed in-band as the first message after open:
//
//   { "jsonrpc": "2.0", "method": "auth.hello",
//     "params": { "token": "<bearer>" }, "id": 1 }
//
// Subsequent calls use a monotonically-increasing id starting at 2.
// The class is transport-agnostic enough to be unit-tested against a
// mock WebSocket constructor (see client.test.ts).

export type JsonRpcId = number;

export interface JsonRpcRequest {
  jsonrpc: "2.0";
  method: string;
  params?: unknown;
  id: JsonRpcId;
}

export interface JsonRpcResponse {
  jsonrpc: "2.0";
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
  id: JsonRpcId;
}

export interface JsonRpcNotification {
  jsonrpc: "2.0";
  method: string;
  params?: unknown;
}

export type Unsubscribe = () => void;
export type NotificationHandler = (n: JsonRpcNotification) => void;

/** Minimal WebSocket surface we depend on — lets us swap a mock. */
export interface WebSocketLike {
  readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((this: WebSocketLike, ev: Event) => unknown) | null;
  onclose: ((this: WebSocketLike, ev: CloseEvent) => unknown) | null;
  onerror: ((this: WebSocketLike, ev: Event) => unknown) | null;
  onmessage: ((this: WebSocketLike, ev: MessageEvent) => unknown) | null;
}

export type WebSocketFactory = (url: string) => WebSocketLike;

export interface JsonRpcWSOptions {
  url: string;
  token?: string;
  /** Override for tests; defaults to global `WebSocket`. */
  factory?: WebSocketFactory;
  /** Initial reconnect backoff (ms). Doubles up to `maxBackoffMs`. */
  initialBackoffMs?: number;
  maxBackoffMs?: number;
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
}

const READY_OPEN = 1;

/**
 * JSON-RPC 2.0 over WebSocket with auto-reconnect and notification
 * subscription. Single-instance assumption: one bridge per UI.
 */
export class JsonRpcWS {
  private readonly url: string;
  private readonly token: string;
  private readonly factory: WebSocketFactory;
  private readonly initialBackoffMs: number;
  private readonly maxBackoffMs: number;

  private socket: WebSocketLike | null = null;
  private nextId: JsonRpcId = 1;
  private readonly pending = new Map<JsonRpcId, Pending>();
  private readonly subscribers = new Set<NotificationHandler>();
  private backoffMs: number;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closedByUser = false;

  public constructor(opts: JsonRpcWSOptions) {
    this.url = opts.url;
    this.token = opts.token ?? "dev-token";
    this.factory =
      opts.factory ??
      ((url: string): WebSocketLike =>
        new WebSocket(url) as unknown as WebSocketLike);
    this.initialBackoffMs = opts.initialBackoffMs ?? 1000;
    this.maxBackoffMs = opts.maxBackoffMs ?? 30_000;
    this.backoffMs = this.initialBackoffMs;
  }

  /** Open the socket; safe to call multiple times. */
  public connect(): void {
    this.closedByUser = false;
    if (this.socket && this.socket.readyState === READY_OPEN) return;
    const ws = this.factory(this.url);
    this.socket = ws;
    ws.onopen = (): void => this.handleOpen();
    ws.onmessage = (ev: MessageEvent): void => this.handleMessage(ev);
    ws.onclose = (): void => this.handleClose();
    ws.onerror = (): void => {
      // Errors will be followed by `onclose`; the reconnect path handles
      // backoff so we just swallow the noisy event.
    };
  }

  /** Permanently close; cancels any pending reconnect. */
  public close(): void {
    this.closedByUser = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.socket) {
      this.socket.close();
      this.socket = null;
    }
    for (const p of this.pending.values()) {
      p.reject(new Error("connection closed"));
    }
    this.pending.clear();
  }

  /**
   * Invoke a JSON-RPC method. Resolves with the `result` field on
   * success; rejects with `Error(message)` on JSON-RPC error or socket
   * teardown.
   */
  public call<T = unknown>(method: string, params?: unknown): Promise<T> {
    const id = this.allocId();
    const req: JsonRpcRequest = {
      jsonrpc: "2.0",
      method,
      params,
      id,
    };
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, {
        resolve: (v: unknown): void => resolve(v as T),
        reject,
      });
      this.send(req);
    });
  }

  /** Register a handler invoked for every server-pushed notification. */
  public subscribe(onNotif: NotificationHandler): Unsubscribe {
    this.subscribers.add(onNotif);
    return (): void => {
      this.subscribers.delete(onNotif);
    };
  }

  private allocId(): JsonRpcId {
    const id = this.nextId;
    this.nextId += 1;
    return id;
  }

  private send(msg: JsonRpcRequest): void {
    if (!this.socket || this.socket.readyState !== READY_OPEN) {
      // Queue would be nice; for v0.1 we reject so the caller knows.
      const pending = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      pending?.reject(new Error("socket not open"));
      return;
    }
    this.socket.send(JSON.stringify(msg));
  }

  private handleOpen(): void {
    this.backoffMs = this.initialBackoffMs;
    // Auth handshake — id=1 per spec. We consume the id slot manually
    // so subsequent `call()` ids start at 2.
    const authId = this.allocId();
    const auth: JsonRpcRequest = {
      jsonrpc: "2.0",
      method: "auth.hello",
      params: { token: this.token },
      id: authId,
    };
    // Auth response is logged but not surfaced (server returns success
    // or closes the socket).
    this.pending.set(authId, {
      resolve: (): void => undefined,
      reject: (): void => undefined,
    });
    if (this.socket) {
      this.socket.send(JSON.stringify(auth));
    }
  }

  private handleMessage(ev: MessageEvent): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(String(ev.data));
    } catch {
      return;
    }
    if (!parsed || typeof parsed !== "object") return;
    const msg = parsed as Partial<JsonRpcResponse & JsonRpcNotification>;
    if (typeof msg.id === "number") {
      // Response.
      const pending = this.pending.get(msg.id);
      if (!pending) return;
      this.pending.delete(msg.id);
      if (msg.error) {
        pending.reject(
          new Error(`${msg.error.code}: ${msg.error.message}`),
        );
      } else {
        pending.resolve(msg.result);
      }
      return;
    }
    if (typeof msg.method === "string") {
      const notif: JsonRpcNotification = {
        jsonrpc: "2.0",
        method: msg.method,
        params: msg.params,
      };
      for (const sub of this.subscribers) sub(notif);
    }
  }

  private handleClose(): void {
    this.socket = null;
    if (this.closedByUser) return;
    // Schedule reconnect with exponential backoff capped at maxBackoffMs.
    const delay = Math.min(this.backoffMs, this.maxBackoffMs);
    this.reconnectTimer = setTimeout((): void => {
      this.reconnectTimer = null;
      this.backoffMs = Math.min(this.backoffMs * 2, this.maxBackoffMs);
      this.connect();
    }, delay);
  }
}
