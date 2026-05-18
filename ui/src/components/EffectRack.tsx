// Per-deck effects rack — renders 3 <EffectSlot/>s side by side and
// owns slot-reordering UX (HTML5 drag-drop + Shift-Up/Shift-Down
// keyboard) on top of the underlying EffectSwapSlots event.
//
// State.decks[deck].effects[3] feeds the slot states directly; the
// manifest comes from `useEffectsManifest()` in the parent. The rack
// stays dumb about effect internals (no fetching, no per-effect RPC) —
// it owns the swap submission + transient drag/over visual highlights.

import { useState, type CSSProperties, type JSX, type DragEvent, type KeyboardEvent } from "react";
import type { JsonRpcWS } from "../ws/client";
import type { DeckId, EffectSlotState } from "../store/engine";
import type { EffectManifest } from "../store/effectsManifest";
import { EffectSlot } from "./EffectSlot";

export interface EffectRackProps {
  deck: DeckId;
  effects: readonly [EffectSlotState, EffectSlotState, EffectSlotState];
  manifest: EffectManifest;
  client: JsonRpcWS;
}

const wrapStyle: CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: 4,
};

const headerStyle: CSSProperties = {
  fontFamily: "monospace",
  fontSize: 11,
  color: "#888",
  letterSpacing: 0.5,
};

const rowStyle: CSSProperties = {
  display: "flex",
  gap: 4,
  alignItems: "stretch",
};

const slotWrapBase: CSSProperties = {
  position: "relative",
  outline: "none",
  borderRadius: 2,
};

// Custom MIME so we don't collide with the browser's "text/plain"
// default + ignore any in-page drags that aren't our slot tokens.
const DRAG_MIME = "application/x-hypehouse-slot";

const NUM_SLOTS = 3;

/** Submit an EffectSwapSlots event over the JSON-RPC bridge. Fire-and-forget. */
const submitSwap = (
  client: JsonRpcWS,
  deck: DeckId,
  slot_a: number,
  slot_b: number,
): void => {
  void client
    .call("submit_event", { EffectSwapSlots: { deck, slot_a, slot_b } })
    .catch((): void => undefined);
};

export const EffectRack = ({
  deck,
  effects,
  manifest,
  client,
}: EffectRackProps): JSX.Element => {
  // Source slot index (the one being dragged). Null when no drag is
  // in progress. Used to gray-out the source card.
  const [dragSource, setDragSource] = useState<number | null>(null);
  // Hover target — the slot the user's pointer is currently over. Used
  // to highlight the drop target. Null when not over a valid slot.
  const [dropTarget, setDropTarget] = useState<number | null>(null);

  const onDragStart =
    (slot: number) =>
    (e: DragEvent<HTMLDivElement>): void => {
      e.dataTransfer.setData(DRAG_MIME, String(slot));
      e.dataTransfer.effectAllowed = "move";
      setDragSource(slot);
    };

  const onDragEnd = (): void => {
    setDragSource(null);
    setDropTarget(null);
  };

  const onDragOver =
    (slot: number) =>
    (e: DragEvent<HTMLDivElement>): void => {
      // Only accept our own MIME — keep arbitrary drags (e.g. files)
      // from highlighting slots they can't drop onto.
      if (!e.dataTransfer.types.includes(DRAG_MIME)) return;
      e.preventDefault(); // required to allow drop
      e.dataTransfer.dropEffect = "move";
      if (dropTarget !== slot) {
        setDropTarget(slot);
      }
    };

  const onDragLeave =
    (slot: number) =>
    (): void => {
      if (dropTarget === slot) {
        setDropTarget(null);
      }
    };

  const onDrop =
    (slot: number) =>
    (e: DragEvent<HTMLDivElement>): void => {
      const raw = e.dataTransfer.getData(DRAG_MIME);
      setDragSource(null);
      setDropTarget(null);
      if (raw === "") return;
      const source = Number(raw);
      if (!Number.isFinite(source)) return;
      if (source === slot) return; // same-slot drop = no-op
      if (source < 0 || source >= NUM_SLOTS) return;
      e.preventDefault();
      submitSwap(client, deck, source, slot);
    };

  // Keyboard reorder: Shift-Up / Shift-Down on a focused slot wrapper
  // swaps with the adjacent slot. Edge slots (top → Shift-Up, bottom
  // → Shift-Down) silently no-op so the gesture is keyboard-safe.
  const onKeyDown =
    (slot: number) =>
    (e: KeyboardEvent<HTMLDivElement>): void => {
      if (!e.shiftKey) return;
      if (e.key === "ArrowUp" && slot > 0) {
        e.preventDefault();
        submitSwap(client, deck, slot, slot - 1);
      } else if (e.key === "ArrowDown" && slot < NUM_SLOTS - 1) {
        e.preventDefault();
        submitSwap(client, deck, slot, slot + 1);
      }
    };

  const slotWrapStyle = (slot: number): CSSProperties => ({
    ...slotWrapBase,
    opacity: dragSource === slot ? 0.4 : 1,
    boxShadow:
      dropTarget === slot && dragSource !== slot
        ? "0 0 0 2px #6cf inset"
        : "none",
    transition: "opacity 80ms, box-shadow 80ms",
  });

  return (
    <div
      style={wrapStyle}
      data-testid={`fx-rack-${deck}`}
      aria-label={`effects-rack-${deck}`}
    >
      <span style={headerStyle}>EFFECTS</span>
      <div style={rowStyle}>
        {effects.map(
          (slot, i): JSX.Element => (
            <div
              key={i}
              draggable
              tabIndex={0}
              role="listitem"
              aria-label={`effects-rack-${deck}-slot-${i}-handle`}
              data-testid={`fx-slot-handle-${deck}-${i}`}
              data-drag-source={dragSource === i ? "true" : undefined}
              data-drop-target={
                dropTarget === i && dragSource !== i ? "true" : undefined
              }
              style={slotWrapStyle(i)}
              onDragStart={onDragStart(i)}
              onDragEnd={onDragEnd}
              onDragOver={onDragOver(i)}
              onDragLeave={onDragLeave(i)}
              onDrop={onDrop(i)}
              onKeyDown={onKeyDown(i)}
            >
              <EffectSlot
                deck={deck}
                slot={i}
                state={slot}
                manifest={manifest}
                client={client}
              />
            </div>
          ),
        )}
      </div>
    </div>
  );
};
