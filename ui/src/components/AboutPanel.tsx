// AboutPanel — engine version + active output device + feature flags.
//
// Backed by the new `engine.session_info` RPC (#144). Read-only,
// fetched once on mount; not subscribed to state_changed because the
// payload doesn't carry session-static fields.
//
// Useful for:
// - Bug reports — operator can copy version + flags into the issue body.
// - Sanity-check on first run — confirms BlackHole / VB-Cable was actually
//   picked up by the engine (otherwise the dropdown's persisted choice
//   silently doesn't apply).

import {
  useEffect,
  useState,
  type CSSProperties,
  type JSX,
} from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  formatCountdownMicros,
  formatRelativeMicros,
  requeueAllPending,
  syncNow,
  useSessionInfo,
  useStemsStatus,
  useSyncStatus,
  type SessionFeatures,
} from "../store/sessionInfo";

export interface AboutPanelProps {
  readonly client: JsonRpcWS;
}

const containerStyle: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: "0.3rem",
  padding: "0.6rem",
  border: "1px solid #2a2a2a",
  borderRadius: "0.4rem",
  background: "#0f0f0f",
  color: "#bbb",
  fontFamily: "system-ui, sans-serif",
  fontSize: "0.8rem",
  maxWidth: "420px",
};

const labelStyle: CSSProperties = {
  fontWeight: 600,
  color: "#888",
  letterSpacing: 0.5,
  fontSize: "0.7rem",
  textTransform: "uppercase",
};

const rowStyle: CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  gap: "0.6rem",
};

const valueStyle: CSSProperties = {
  color: "#ddd",
  fontFamily: "monospace",
  fontSize: "0.75rem",
};

const syncRowStyle: CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  alignItems: "center",
  gap: "0.6rem",
};

const syncButtonStyle: CSSProperties = {
  background: "#1a2a3a",
  color: "#9cd0e0",
  border: "1px solid #2a3f55",
  padding: "2px 8px",
  borderRadius: "0.25rem",
  fontSize: "0.7rem",
  cursor: "pointer",
  fontFamily: "inherit",
};

const syncButtonBusyStyle: CSSProperties = {
  ...syncButtonStyle,
  cursor: "wait",
  opacity: 0.6,
};

const syncErrorStyle: CSSProperties = {
  color: "#e09c9c",
  fontSize: "0.7rem",
  marginTop: "0.1rem",
};

const syncCountsStyle: CSSProperties = {
  color: "#8a8a8a",
  fontSize: "0.7rem",
  marginTop: "0.1rem",
  marginLeft: "0.4rem",
  fontFamily: "monospace",
};

const flagsRowStyle: CSSProperties = {
  display: "flex",
  flexWrap: "wrap",
  gap: "0.3rem",
  marginTop: "0.2rem",
};

const flagOnStyle: CSSProperties = {
  background: "#1f3d22",
  color: "#9ce0a3",
  border: "1px solid #2c5630",
  padding: "1px 6px",
  borderRadius: "0.25rem",
  fontSize: "0.7rem",
};

const flagOffStyle: CSSProperties = {
  background: "#1a1a1a",
  color: "#666",
  border: "1px solid #2a2a2a",
  padding: "1px 6px",
  borderRadius: "0.25rem",
  fontSize: "0.7rem",
};

const FEATURE_LABELS: ReadonlyArray<[keyof SessionFeatures, string]> = [
  ["midi_clock_in", "MIDI Clock In"],
  ["midi_clock_out", "MIDI Clock Out"],
  ["ableton_link", "Ableton Link"],
  ["sentry_telemetry", "Sentry"],
  ["recording_enabled", "Recording"],
  ["rate_limit_disabled", "Rate-limit OFF"],
  ["shared_ci_runner", "Shared CI"],
];

export const AboutPanel = ({ client }: AboutPanelProps): JSX.Element => {
  const info = useSessionInfo(client);
  const sync = useSyncStatus(client);
  const stems = useStemsStatus(client);
  const [syncing, setSyncing] = useState(false);
  const [syncError, setSyncError] = useState<string>("");
  // Re-render once per second so the countdown ticks down without
  // refetching the RPC. The store still polls every 5s for absolute
  // status updates; this just animates the relative-time string.
  const [, setTick] = useState(0);
  useEffect((): (() => void) => {
    const id = window.setInterval((): void => setTick((n) => n + 1), 1000);
    return (): void => window.clearInterval(id);
  }, []);
  const countdown = formatCountdownMicros(sync.next_sync_micros);
  const onSyncNow = async (): Promise<void> => {
    if (syncing) return;
    setSyncing(true);
    setSyncError("");
    try {
      await syncNow(client);
    } catch (e) {
      // RPC error surface — `-32000` = "cloud sync not configured" /
      // `-32603` = transport or DB failure. We render either inline
      // so the operator knows the click landed but the sync didn't.
      const message = e instanceof Error ? e.message : String(e);
      setSyncError(message);
    } finally {
      setSyncing(false);
    }
  };
  const [queueAllBusy, setQueueAllBusy] = useState(false);
  const [queueAllToast, setQueueAllToast] = useState<string>("");
  // Auto-dismiss the success toast after a short window so a stale
  // "N queued" message doesn't sit next to a fresh "last sync"
  // value. Errors land in the persistent sync-error region instead.
  useEffect((): (() => void) => {
    if (queueAllToast === "") return (): void => {};
    const id = window.setTimeout(
      (): void => setQueueAllToast(""),
      4000,
    );
    return (): void => window.clearTimeout(id);
  }, [queueAllToast]);
  const onQueueAll = async (): Promise<void> => {
    if (queueAllBusy) return;
    setQueueAllBusy(true);
    setQueueAllToast("");
    setSyncError("");
    try {
      const queued = await requeueAllPending(client);
      setQueueAllToast(`${queued} queued for sync`);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setSyncError(message);
    } finally {
      setQueueAllBusy(false);
    }
  };
  const deviceLabel =
    info.output_device_substring === ""
      ? "(system default)"
      : info.output_device_substring;
  const versionLabel = info.version === "" ? "(loading…)" : info.version;

  return (
    <div style={containerStyle} data-testid="about-panel">
      <div style={rowStyle}>
        <span style={labelStyle}>Engine</span>
        <span style={valueStyle} data-testid="about-version">
          v{versionLabel}
        </span>
      </div>
      <div style={rowStyle}>
        <span style={labelStyle}>Audio sink</span>
        <span style={valueStyle} data-testid="about-output-device">
          {deviceLabel}
        </span>
      </div>
      <div style={rowStyle}>
        <span style={labelStyle}>Library</span>
        <span style={valueStyle} data-testid="about-library-count">
          {sync.library_track_count} tracks
          {sync.pending_push_count > 0
            ? ` · ${sync.pending_push_count} pending sync`
            : ""}
        </span>
      </div>
      <div style={syncRowStyle}>
        <span style={labelStyle}>Last sync</span>
        <span
          style={{ display: "flex", alignItems: "center", gap: "0.4rem" }}
        >
          <span style={valueStyle} data-testid="about-last-sync">
            {formatRelativeMicros(sync.last_pull_micros)}
            {countdown !== "" ? ` · next in ${countdown}` : ""}
            {sync.last_tick_error !== ""
              ? ` · ${sync.last_tick_error}`
              : ""}
          </span>
          <button
            type="button"
            data-testid="about-sync-now"
            onClick={onSyncNow}
            disabled={syncing}
            style={syncing ? syncButtonBusyStyle : syncButtonStyle}
            aria-label="Force sync now"
          >
            {syncing ? "syncing…" : "sync now"}
          </button>
          <button
            type="button"
            data-testid="about-queue-all"
            onClick={onQueueAll}
            disabled={queueAllBusy}
            style={queueAllBusy ? syncButtonBusyStyle : syncButtonStyle}
            aria-label="Queue every local track for cloud push"
            title="Queue every local track for cloud push (use after first cloud-sync setup)"
          >
            {queueAllBusy ? "queueing…" : "queue all"}
          </button>
        </span>
      </div>
      {queueAllToast !== "" ? (
        <div style={syncCountsStyle} data-testid="about-queue-all-toast">
          {queueAllToast}
        </div>
      ) : null}
      {syncError !== "" ? (
        <div style={syncErrorStyle} data-testid="about-sync-error">
          {syncError}
        </div>
      ) : null}
      {sync.last_pull_fetched +
        sync.last_pull_applied +
        sync.last_push_pushed >
      0 ? (
        <div style={syncCountsStyle} data-testid="about-sync-counts">
          ↓ {sync.last_pull_fetched} fetched
          {sync.last_pull_applied > 0
            ? ` · ${sync.last_pull_applied} applied`
            : ""}
          {sync.last_push_pushed > 0
            ? ` · ↑ ${sync.last_push_pushed} pushed`
            : ""}
        </div>
      ) : null}
      {stems.ready + stems.pending + stems.failed > 0 ? (
        <div style={rowStyle}>
          <span style={labelStyle}>Stems</span>
          <span style={valueStyle} data-testid="about-stems-status">
            {stems.ready} ready
            {stems.pending > 0 ? ` · ${stems.pending} pending` : ""}
            {stems.failed > 0 ? ` · ${stems.failed} failed` : ""}
          </span>
        </div>
      ) : null}
      <div style={flagsRowStyle} data-testid="about-flags">
        {FEATURE_LABELS.map(([key, label]): JSX.Element => {
          const enabled = info.features[key];
          return (
            <span
              key={key}
              style={enabled ? flagOnStyle : flagOffStyle}
              data-testid={`about-flag-${key}`}
              title={`${label}: ${enabled ? "on" : "off"}`}
            >
              {label}
            </span>
          );
        })}
      </div>
    </div>
  );
};
