// WS connection-state store.
//
// Subscribes to `JsonRpcWS.onOpen` + `onClose` (added in #207 / this
// PR) and exposes a `useConnection(client)` hook returning the live
// state. AboutPanel renders an "engine offline" badge while
// disconnected; future widgets (deck controls, library actions) can
// also gray out when offline.
//
// Why not roll this into `sessionInfo.ts`? Session info is the
// engine's session-static payload (version, flags); connection state
// is the WS transport layer. Different lifetimes, different
// concerns — keep them separate.

import { useEffect, useSyncExternalStore } from "react";
import type { JsonRpcWS } from "../ws/client";

export type ConnectionState = "open" | "closed";

let current: ConnectionState = "closed";
const listeners: Set<() => void> = new Set();

const notify = (): void => {
  for (const l of listeners) l();
};
const subscribe = (l: () => void): (() => void) => {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
};
const getSnapshot = (): ConnectionState => current;

/** Test seam. */
export const __setConnectionState = (next: ConnectionState): void => {
  current = next;
  notify();
};

/** Reset — test only. */
export const __resetConnectionState = (): void => {
  current = "closed";
  notify();
};

/**
 * React hook returning the live WS connection state. Subscribes to
 * `client.onOpen` + `client.onClose` on mount; falls back to the
 * client's instantaneous `isOpen()` for the initial render before
 * any event fires.
 */
export const useConnection = (client: JsonRpcWS): ConnectionState => {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  useEffect(() => {
    // Initialize from the client's current state — covers the case
    // where the socket opened before the hook mounted.
    const isOpen = (
      client as { isOpen?: () => boolean }
    ).isOpen;
    if (typeof isOpen === "function" && isOpen.call(client)) {
      __setConnectionState("open");
    }
    const onOpen = (
      client as { onOpen?: (cb: () => void) => () => void }
    ).onOpen;
    const onClose = (
      client as { onClose?: (cb: () => void) => () => void }
    ).onClose;
    const unsubs: Array<() => void> = [];
    if (typeof onOpen === "function") {
      unsubs.push(
        onOpen.call(client, (): void => __setConnectionState("open")),
      );
    }
    if (typeof onClose === "function") {
      unsubs.push(
        onClose.call(client, (): void => __setConnectionState("closed")),
      );
    }
    return (): void => {
      for (const u of unsubs) u();
    };
  }, [client]);
  return snapshot;
};
