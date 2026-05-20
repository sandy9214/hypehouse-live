// LibraryFilters.tsx — smart-filter chip bar above the library list.
//
// Two filters: (1) BPM range dual-handle slider, clamped [60, 200];
// (2) "Compatible with..." track picker (search-as-you-type) that
// sends `compatible_with_track_id` so copilot post-filters by
// `camelot_distance ≤ 2` (= mashup-friendly envelope).
// Active filters render as removable chips. Component is
// presentational over the store; Library.tsx owns re-search effect.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ChangeEvent, CSSProperties, JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import {
  EMPTY_FILTERS,
  hasActiveFilters,
  searchLibrary,
  setLibraryFilters,
  useLibraryFilters,
  type LibraryFilters as LibraryFiltersState,
  type LibraryTrack,
} from "../store/library";

const BPM_MIN_BOUND = 60;
const BPM_MAX_BOUND = 200;
const PICKER_DEBOUNCE_MS = 200;
const PICKER_MAX_RESULTS = 8;

// prettier-ignore
const STYLE: Record<string, CSSProperties> = {
  container: { display: "flex", flexWrap: "wrap", alignItems: "center", gap: 12, padding: "6px 8px", borderBottom: "1px solid #1c1c1c", background: "#0a0a0a", fontFamily: "monospace", fontSize: 12, color: "#ccc" },
  group: { display: "flex", alignItems: "center", gap: 6 },
  label: { opacity: 0.6, textTransform: "uppercase", letterSpacing: 1 },
  sliderWrap: { position: "relative", width: 140, height: 24, display: "inline-block" },
  track: { position: "absolute", top: 11, left: 0, right: 0, height: 2, background: "#333", borderRadius: 1 },
  rangeInput: { position: "absolute", top: 0, width: "100%", height: 24, margin: 0, background: "transparent", pointerEvents: "auto", WebkitAppearance: "none" },
  picker: { background: "#000", color: "#fff", border: "1px solid #333", borderRadius: 4, padding: "3px 6px", fontFamily: "monospace", fontSize: 12, width: 160 },
  dropdown: { position: "absolute", top: "100%", left: 0, background: "#111", border: "1px solid #333", borderRadius: 4, marginTop: 2, maxHeight: 200, overflowY: "auto", zIndex: 10, minWidth: 200 },
  dropdownItem: { padding: "4px 8px", cursor: "pointer", borderBottom: "1px solid #1c1c1c" },
  chip: { display: "inline-flex", alignItems: "center", gap: 4, background: "#1c2a3a", color: "#cde", border: "1px solid #2c4a6a", borderRadius: 12, padding: "2px 8px", fontSize: 11 },
  chipClose: { background: "transparent", color: "inherit", border: "none", cursor: "pointer", fontSize: 14, lineHeight: 1, padding: 0 },
  clearAll: { background: "transparent", color: "#888", border: "none", cursor: "pointer", fontSize: 11, textDecoration: "underline", padding: 0 },
  chipsRow: { display: "flex", gap: 6, flexWrap: "wrap" },
};

const clampBpm = (v: number): number =>
  Math.max(BPM_MIN_BOUND, Math.min(BPM_MAX_BOUND, Math.round(v)));

export interface LibraryFiltersProps {
  client: JsonRpcWS;
  // 200ms is the default debounce on the autocomplete picker — tests
  // override to 0 so the dropdown populates synchronously.
  pickerDebounceMs?: number;
}

export const LibraryFilters = ({
  client,
  pickerDebounceMs = PICKER_DEBOUNCE_MS,
}: LibraryFiltersProps): JSX.Element => {
  const filters = useLibraryFilters();
  const [pickerQuery, setPickerQuery] = useState<string>("");
  const [pickerResults, setPickerResults] = useState<
    ReadonlyArray<LibraryTrack>
  >([]);
  const [pickerOpen, setPickerOpen] = useState<boolean>(false);
  const debounceTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const update = useCallback(
    (patch: Partial<LibraryFiltersState>): void => {
      setLibraryFilters({ ...filters, ...patch });
    },
    [filters],
  );

  // Lower bound defaults to BPM_MIN_BOUND when filter is null so the
  // slider knob has a visible position; the chip render keys off the
  // raw filter value so "no chip" survives the unset state.
  const lo = filters.bpmMin ?? BPM_MIN_BOUND;
  const hi = filters.bpmMax ?? BPM_MAX_BOUND;

  const onLoChange = (e: ChangeEvent<HTMLInputElement>): void => {
    const next = Math.min(clampBpm(Number(e.target.value)), hi);
    update({ bpmMin: next === BPM_MIN_BOUND ? null : next });
  };
  const onHiChange = (e: ChangeEvent<HTMLInputElement>): void => {
    const next = Math.max(clampBpm(Number(e.target.value)), lo);
    update({ bpmMax: next === BPM_MAX_BOUND ? null : next });
  };

  // Picker debounce: search-as-you-type for the reference track.
  useEffect((): (() => void) => {
    if (debounceTimer.current !== null) {
      clearTimeout(debounceTimer.current);
      debounceTimer.current = null;
    }
    const q = pickerQuery.trim();
    if (!q) {
      setPickerResults([]);
      return (): void => undefined;
    }
    debounceTimer.current = setTimeout((): void => {
      void searchLibrary(client, q, { limit: PICKER_MAX_RESULTS }).then(
        (rows: ReadonlyArray<LibraryTrack>): void => {
          setPickerResults(rows.slice(0, PICKER_MAX_RESULTS));
        },
      );
    }, pickerDebounceMs);
    return (): void => {
      if (debounceTimer.current !== null) {
        clearTimeout(debounceTimer.current);
        debounceTimer.current = null;
      }
    };
  }, [pickerQuery, client, pickerDebounceMs]);

  const selectCompatibleWith = (t: LibraryTrack): void => {
    update({ compatibleWithTrackId: t.id });
    setPickerQuery("");
    setPickerOpen(false);
    setPickerResults([]);
  };

  const compatLabel = useMemo<string>(
    (): string => filters.compatibleWithTrackId ?? "",
    [filters.compatibleWithTrackId],
  );

  const clearAll = (): void => {
    setLibraryFilters(EMPTY_FILTERS);
    setPickerQuery("");
    setPickerResults([]);
  };

  return (
    <div
      data-testid="library-filters"
      role="region"
      aria-label="Library filters"
      style={STYLE.container}
    >
      <div style={STYLE.group}>
        <span style={STYLE.label}>BPM</span>
        <span data-testid="library-filter-bpm-lo">{lo}</span>
        <div style={STYLE.sliderWrap}>
          <div style={STYLE.track} />
          <input
            type="range" data-testid="library-filter-bpm-min"
            aria-label="Minimum BPM" min={BPM_MIN_BOUND} max={BPM_MAX_BOUND}
            value={lo} onChange={onLoChange} style={STYLE.rangeInput}
          />
          <input
            type="range" data-testid="library-filter-bpm-max"
            aria-label="Maximum BPM" min={BPM_MIN_BOUND} max={BPM_MAX_BOUND}
            value={hi} onChange={onHiChange} style={STYLE.rangeInput}
          />
        </div>
        <span data-testid="library-filter-bpm-hi">{hi}</span>
      </div>

      <div style={{ ...STYLE.group, position: "relative" }}>
        <span style={STYLE.label}>Compat</span>
        <input
          type="search" data-testid="library-filter-compat-input"
          aria-label="Find compatible-with reference track"
          placeholder="track id..." value={pickerQuery}
          onChange={(e): void => {
            setPickerQuery(e.target.value);
            setPickerOpen(true);
          }}
          onFocus={(): void => setPickerOpen(true)}
          style={STYLE.picker}
        />
        {pickerOpen && pickerResults.length > 0 && (
          <div
            data-testid="library-filter-compat-dropdown"
            role="listbox" style={STYLE.dropdown}
          >
            {pickerResults.map(
              (t: LibraryTrack): JSX.Element => (
                <div
                  key={t.id} role="option" aria-selected={false}
                  data-testid={`library-filter-compat-option-${t.id}`}
                  onClick={(): void => selectCompatibleWith(t)}
                  style={STYLE.dropdownItem}
                >
                  {t.id}{" "}
                  <span style={{ opacity: 0.5 }}>({t.camelot_key})</span>
                </div>
              ),
            )}
          </div>
        )}
      </div>

      <div style={STYLE.group}>
        <label
          style={{
            display: "inline-flex",
            alignItems: "center",
            gap: 4,
            cursor: "pointer",
          }}
        >
          <input
            type="checkbox"
            data-testid="library-filter-pending-toggle"
            aria-label="Show only tracks awaiting cloud sync"
            checked={filters.pendingSyncOnly}
            onChange={(e): void =>
              update({ pendingSyncOnly: e.target.checked })
            }
          />
          <span style={STYLE.label}>Pending sync</span>
        </label>
      </div>

      {hasActiveFilters(filters) && (
        <div data-testid="library-filter-chips" style={STYLE.chipsRow}>
          {(filters.bpmMin !== null || filters.bpmMax !== null) && (
            <span style={STYLE.chip} data-testid="library-filter-chip-bpm">
              BPM {filters.bpmMin ?? BPM_MIN_BOUND}-
              {filters.bpmMax ?? BPM_MAX_BOUND}
              <button
                type="button" aria-label="Remove BPM range filter"
                data-testid="library-filter-chip-bpm-clear"
                onClick={(): void => update({ bpmMin: null, bpmMax: null })}
                style={STYLE.chipClose}
              >
                ×
              </button>
            </span>
          )}
          {filters.compatibleWithTrackId !== null && (
            <span style={STYLE.chip} data-testid="library-filter-chip-compat">
              compat {compatLabel}
              <button
                type="button" aria-label="Remove compatible-with filter"
                data-testid="library-filter-chip-compat-clear"
                onClick={(): void =>
                  update({ compatibleWithTrackId: null })
                }
                style={STYLE.chipClose}
              >
                ×
              </button>
            </span>
          )}
          {filters.pendingSyncOnly && (
            <span
              style={STYLE.chip}
              data-testid="library-filter-chip-pending"
            >
              pending sync only
              <button
                type="button"
                aria-label="Remove pending-sync-only filter"
                data-testid="library-filter-chip-pending-clear"
                onClick={(): void =>
                  update({ pendingSyncOnly: false })
                }
                style={STYLE.chipClose}
              >
                ×
              </button>
            </span>
          )}
          <button
            type="button" data-testid="library-filter-clear-all"
            onClick={clearAll} style={STYLE.clearAll}
          >
            clear all
          </button>
        </div>
      )}
    </div>
  );
};
