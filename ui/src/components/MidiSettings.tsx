// MidiSettings — operator panel for choosing / importing / exporting
// MIDI + keyboard mappings. Lists built-ins (ddj200, keyboard) plus
// every custom JSON the user has imported, highlights the active one,
// and wires Reload to WebMIDIListener.applyMapping for live hot-swap
// without restarting the bridge or reacquiring MIDIAccess.
//
// Error reporting reuses the engine.decode_error Toaster channel — a
// failed schema validation produces a toast with category
// `mapping_error` so the same visual treatment surfaces both engine
// decode failures and mapping import problems.

import { type CSSProperties, useCallback, useEffect, useRef, useState } from "react";

import type { WebMIDIListener } from "../midi/listener.ts";
import {
  deleteMapping,
  getActiveMappingName,
  listMappings,
  loadMapping,
  type ListenerKind,
  type MappingEntry,
  persistMapping,
  setActiveMappingName,
  validateMapping,
} from "../midi/MappingStore.ts";
import { applyDecodeErrorNotification } from "../store/notifications";

export interface MidiSettingsProps {
  listener?: WebMIDIListener | null;
  kind?: ListenerKind;
}

const S: Record<string, CSSProperties> = {
  panel: {
    background: "#0d0d10", border: "1px solid #2a2a30", borderRadius: 6,
    padding: 12, fontFamily: "monospace", color: "#d8d8e0",
    minWidth: 340, maxWidth: 520,
  },
  row: {
    display: "flex", alignItems: "center", gap: 8,
    padding: "6px 4px", borderBottom: "1px solid #1f1f25",
  },
  active: { background: "#1a1f2a" },
  btn: {
    background: "#1c1c22", border: "1px solid #3a3a44", color: "#d8d8e0",
    fontFamily: "monospace", fontSize: 12, padding: "4px 8px",
    cursor: "pointer", borderRadius: 3,
  },
  header: { fontWeight: "bold", marginBottom: 8, fontSize: 13 },
  tag: { fontSize: 10, opacity: 0.6, marginLeft: 6 },
};

const reportError = (msg: string): void => {
  applyDecodeErrorNotification({
    jsonrpc: "2.0",
    method: "engine.decode_error",
    params: { deck: "A", track_id: "midi-mapping", category: "mapping_error", error: msg },
  });
};

/** Trigger a browser file download. Module-scope so tests can intercept
 *  via spying on URL.createObjectURL / HTMLAnchorElement.click. */
const triggerDownload = (filename: string, content: string): void => {
  const blob = new Blob([content], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.style.display = "none";
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
};

export const MidiSettings = ({
  listener = null, kind = "midi",
}: MidiSettingsProps): JSX.Element => {
  const [entries, setEntries] = useState<MappingEntry[]>(() => listMappings());
  const [active, setActive] = useState<string>(() => getActiveMappingName(kind));
  const fileInputRef = useRef<HTMLInputElement | null>(null);

  const refresh = useCallback((): void => {
    setEntries(listMappings());
    setActive(getActiveMappingName(kind));
  }, [kind]);

  useEffect(refresh, [refresh]);

  const visible = entries.filter((e) => e.kind === kind);

  const onActivate = useCallback((name: string): void => {
    const mapping = loadMapping(name);
    if (!mapping) { reportError(`mapping '${name}' not found`); return; }
    if (kind === "midi" && listener) {
      const result = listener.applyMapping(mapping);
      if (!result.ok) { reportError(result.error ?? "applyMapping failed"); return; }
    }
    setActiveMappingName(kind, name);
    setActive(name);
  }, [kind, listener]);

  const onFileChosen = (e: React.ChangeEvent<HTMLInputElement>): void => {
    const file = e.target.files?.[0];
    e.target.value = "";
    if (!file) return;
    file.text().then((text): void => {
      let parsed: unknown;
      try { parsed = JSON.parse(text); }
      catch (err) { reportError(`invalid JSON: ${String(err)}`); return; }
      const validation = validateMapping(parsed);
      if (!validation.ok) { reportError(validation.error ?? "mapping validation failed"); return; }
      const id = (parsed as { id: string }).id;
      const result = persistMapping(id, parsed);
      if (!result.ok) { reportError(result.error ?? "persist failed"); return; }
      refresh();
    }).catch((err: unknown): void => { reportError(`file read failed: ${String(err)}`); });
  };

  const onExport = (name: string): void => {
    const mapping = loadMapping(name);
    if (!mapping) { reportError(`cannot export '${name}': not found`); return; }
    triggerDownload(`${name}.json`, JSON.stringify(mapping, null, 2));
  };

  const onDelete = (name: string): void => {
    if (!deleteMapping(name)) {
      reportError(`cannot delete '${name}' (built-in or storage error)`);
      return;
    }
    refresh();
  };

  return (
    <div data-testid="midi-settings" style={S.panel}>
      <div style={S.header}>
        MIDI mappings ({kind === "midi" ? "controllers" : "keyboard"})
      </div>
      <div data-testid="mapping-list">
        {visible.map((entry): JSX.Element => {
          const isActive = entry.name === active;
          return (
            <div
              key={entry.name}
              data-testid={`mapping-row-${entry.name}`}
              data-active={isActive ? "true" : "false"}
              style={isActive ? { ...S.row, ...S.active } : S.row}
            >
              <button
                type="button"
                aria-label={`Activate ${entry.name}`}
                onClick={(): void => onActivate(entry.name)}
                style={{ ...S.btn, flex: "1 1 auto", textAlign: "left" }}
              >
                <span>{entry.name}</span>
                <span style={S.tag}>{entry.builtin ? "built-in" : "custom"}</span>
                {isActive ? <span style={S.tag}>active</span> : null}
              </button>
              <button type="button" aria-label={`Reload ${entry.name}`}
                onClick={(): void => onActivate(entry.name)} style={S.btn}>
                reload
              </button>
              <button type="button" aria-label={`Export ${entry.name}`}
                onClick={(): void => onExport(entry.name)} style={S.btn}>
                export
              </button>
              {entry.builtin ? null : (
                <button type="button" aria-label={`Delete ${entry.name}`}
                  onClick={(): void => onDelete(entry.name)} style={S.btn}>
                  delete
                </button>
              )}
            </div>
          );
        })}
      </div>
      <div style={{ marginTop: 10, display: "flex", gap: 8 }}>
        <button
          type="button"
          onClick={(): void => fileInputRef.current?.click()}
          aria-label="Import custom mapping"
          style={S.btn}
        >
          Import custom…
        </button>
        <input
          ref={fileInputRef}
          type="file"
          accept="application/json,.json"
          aria-label="Mapping file picker"
          data-testid="mapping-file-input"
          style={{ display: "none" }}
          onChange={onFileChosen}
        />
      </div>
    </div>
  );
};
