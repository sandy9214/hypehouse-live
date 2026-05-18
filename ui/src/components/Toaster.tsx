// Toaster — bottom-right toast stack for engine.decode_error events.
//
// Subscribes to the `notifications` store (which buffers
// `engine.decode_error` notifications fanned out by the bridge) and
// renders the most recent N=3 as stacked toasts. Each toast carries
// an `X` button for manual dismissal; the store auto-evicts after 5s
// regardless.
//
// Styling deliberately matches the rest of the engine UI's minimal
// monospaced palette — dark surface, red error border, subtle alert
// glyph. The stack lives in a fixed-position container at the
// bottom-right corner so it never displaces the deck layout.

import type { CSSProperties } from "react";
import {
  type DecodeErrorNotification,
  dismissDecodeError,
  useDecodeErrors,
} from "../store/notifications";

const MAX_VISIBLE = 3;

const containerStyle: CSSProperties = {
  position: "fixed",
  right: 16,
  bottom: 16,
  display: "flex",
  flexDirection: "column",
  alignItems: "flex-end",
  gap: 8,
  zIndex: 9999,
  pointerEvents: "none", // each toast re-enables its own pointer events
};

const toastStyle: CSSProperties = {
  pointerEvents: "auto",
  minWidth: 280,
  maxWidth: 380,
  background: "#1a0a0a",
  border: "1px solid #6e1f1f",
  color: "#ffd0d0",
  fontFamily: "monospace",
  fontSize: 12,
  lineHeight: 1.4,
  padding: "8px 10px",
  borderRadius: 4,
  boxShadow: "0 2px 8px rgba(0, 0, 0, 0.6)",
  display: "flex",
  alignItems: "flex-start",
  gap: 8,
};

const iconStyle: CSSProperties = {
  flex: "0 0 auto",
  width: 18,
  height: 18,
  borderRadius: "50%",
  background: "#6e1f1f",
  color: "#ffd0d0",
  fontWeight: "bold",
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  fontSize: 12,
};

const bodyStyle: CSSProperties = {
  flex: "1 1 auto",
  minWidth: 0,
  wordBreak: "break-word",
};

const headerStyle: CSSProperties = {
  fontWeight: "bold",
  color: "#ff8a8a",
  marginBottom: 2,
};

const dismissButtonStyle: CSSProperties = {
  flex: "0 0 auto",
  background: "transparent",
  border: "none",
  color: "#ffd0d0",
  cursor: "pointer",
  fontFamily: "monospace",
  fontSize: 14,
  padding: 0,
  marginLeft: 4,
  lineHeight: 1,
};

const categoryLabel = (category: string): string => {
  switch (category) {
    case "file_not_found":
      return "File not found";
    case "format_unsupported":
      return "Format unsupported";
    case "decoder_error":
      return "Decoder error";
    case "resource_exhausted":
      return "No free decode slot";
    case "unknown_inline_source":
      return "Unknown source";
    case "decoder_thread_spawn":
      return "Decoder thread spawn failed";
    // PR #56 follow-up — surface mid-stream / decoder-thread-panic
    // events with distinct user-facing copy.
    case "mid_stream_decode_failure":
      return "Decode failed mid-stream";
    case "decoder_thread_panic":
      return "Decoder thread crashed";
    default:
      return "Decode error";
  }
};

interface ToastItemProps {
  notification: DecodeErrorNotification;
}

const ToastItem = ({ notification }: ToastItemProps): JSX.Element => {
  const onDismiss = (): void => dismissDecodeError(notification.id);
  return (
    <div
      role="alert"
      aria-live="assertive"
      data-testid={`toast-${notification.id}`}
      data-deck={notification.deck}
      data-category={notification.category}
      style={toastStyle}
    >
      <span aria-hidden="true" style={iconStyle}>
        !
      </span>
      <div style={bodyStyle}>
        <div style={headerStyle}>
          {categoryLabel(notification.category)} (Deck {notification.deck})
        </div>
        <div data-testid={`toast-${notification.id}-error`}>
          {notification.error}
        </div>
        <div
          data-testid={`toast-${notification.id}-track-id`}
          style={{ opacity: 0.7, marginTop: 2 }}
        >
          track: {notification.track_id}
        </div>
      </div>
      <button
        type="button"
        aria-label="Dismiss notification"
        data-testid={`toast-${notification.id}-dismiss`}
        onClick={onDismiss}
        style={dismissButtonStyle}
      >
        ×
      </button>
    </div>
  );
};

/**
 * Renders the live queue of decode-error toasts at the bottom-right of
 * the viewport. Returns `null` when no errors are queued so React
 * doesn't introduce an empty wrapper element on the happy path.
 */
export const Toaster = (): JSX.Element | null => {
  const errors = useDecodeErrors();
  if (errors.length === 0) return null;
  // Newest first so the most-recent error is closest to the operator's
  // attention; slice to the visible cap.
  const visible = errors
    .slice()
    .reverse()
    .slice(0, MAX_VISIBLE);
  return (
    <div data-testid="toaster-root" style={containerStyle}>
      {visible.map((n): JSX.Element => (
        <ToastItem key={n.id} notification={n} />
      ))}
    </div>
  );
};
