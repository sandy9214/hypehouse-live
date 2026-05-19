// OutputDevicePicker — settings dropdown for the engine's output device.
//
// Lists devices from `engine.list_output_devices` and lets the user pin
// a target substring (e.g. "BlackHole", "VB-Cable", "pipewire-loopback")
// to localStorage. The engine reads `HYPEHOUSE_OUTPUT_DEVICE` at startup
// — live hot-swap is deferred (ADR-TBD). So we render a "Restart engine
// to apply" hint when the selection differs from the currently-active
// device.
//
// Wire surface:
//   * `engine.list_output_devices` → `{ devices: [{ name, is_default }] }`
//   * localStorage `hypehouse:outputDeviceSubstring`
//
// Software-only positioning (issues #111, #115, #117): streamers route
// engine output into a virtual loopback sink → OBS / Twitch capture
// lossless audio without screen-share loopback.

import { useState, type ChangeEvent, type CSSProperties, type JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  getSelectedDeviceSubstring,
  matchSelectedDevice,
  setSelectedDeviceSubstring,
  useOutputDevices,
  type OutputDeviceList,
} from "../store/outputDevices";

export interface OutputDevicePickerProps {
  readonly client: JsonRpcWS;
}

const containerStyle: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: "0.4rem",
  padding: "0.6rem",
  border: "1px solid #2a2a2a",
  borderRadius: "0.4rem",
  background: "#111",
  color: "#ddd",
  fontFamily: "system-ui, sans-serif",
  fontSize: "0.85rem",
  maxWidth: "320px",
};

const labelStyle: CSSProperties = {
  fontWeight: 600,
  color: "#aaa",
};

const selectStyle: CSSProperties = {
  background: "#1a1a1a",
  color: "#ddd",
  border: "1px solid #2a2a2a",
  borderRadius: "0.25rem",
  padding: "0.3rem 0.4rem",
  fontFamily: "inherit",
  fontSize: "inherit",
};

const hintStyle: CSSProperties = {
  fontSize: "0.75rem",
  color: "#888",
};

const restartHintStyle: CSSProperties = {
  fontSize: "0.75rem",
  color: "#e0a800",
  marginTop: "0.2rem",
};

const SENTINEL_DEFAULT = "__default__";

const isCurrentlyActive = (
  list: OutputDeviceList,
  selected: string,
): boolean => {
  if (selected === "") {
    // Default selection — active device IS the host default.
    return true;
  }
  const matched = matchSelectedDevice(list, selected);
  if (!matched) {
    // Stale or typo'd substring — engine fell back to default.
    return false;
  }
  // Engine is restarted with the env var set — picked device is the
  // matched one. We can't know whether the engine has restarted since
  // the user clicked, so any non-empty selection means "user wanted
  // override; verify by checking the manifest entry vs is_default".
  // is_default = true on the matched entry only when the OS *also*
  // happens to default to that device, which is rare for a loopback.
  return false;
};

export const OutputDevicePicker = ({
  client,
}: OutputDevicePickerProps): JSX.Element => {
  const devices = useOutputDevices(client);
  const [selected, setSelected] = useState<string>(getSelectedDeviceSubstring);

  const handleChange = (event: ChangeEvent<HTMLSelectElement>): void => {
    const value = event.target.value;
    if (value === SENTINEL_DEFAULT) {
      setSelected("");
      setSelectedDeviceSubstring("");
    } else {
      setSelected(value);
      setSelectedDeviceSubstring(value);
    }
  };

  const dropdownValue = selected === "" ? SENTINEL_DEFAULT : selected;
  const restartHint = !isCurrentlyActive(devices, selected);

  return (
    <div style={containerStyle} data-testid="output-device-picker">
      <label htmlFor="hh-output-device" style={labelStyle}>
        Audio output
      </label>
      <select
        id="hh-output-device"
        value={dropdownValue}
        onChange={handleChange}
        style={selectStyle}
        aria-label="Output audio device"
      >
        <option value={SENTINEL_DEFAULT}>System default</option>
        {devices.map((d) => (
          <option key={d.name} value={d.name}>
            {d.name}
            {d.is_default ? " (default)" : ""}
          </option>
        ))}
      </select>
      {devices.length === 0 ? (
        <span style={hintStyle}>No devices reported by the engine yet.</span>
      ) : (
        <span style={hintStyle}>
          Pick a virtual sink (BlackHole / VB-Cable / pipewire-loopback) to
          route the master mix into your livestream encoder.
        </span>
      )}
      {restartHint ? (
        <span style={restartHintStyle} data-testid="output-device-restart-hint">
          Restart the engine for the new device to take effect.
        </span>
      ) : null}
    </div>
  );
};
