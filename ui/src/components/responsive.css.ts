// responsive.css.ts — CSS string used by DeckRow to scope responsive
// behaviour (knob scaling, effects-rack stacking, library drawer
// overlay) to the < 768 px viewport. Inline styles can't express media
// queries, so we inject this string into a <style> tag inside the
// component tree. Lives in its own module so DeckRow.tsx stays under
// the 250-line per-component budget.
//
// Scopes:
//   .hh-responsive-root — wraps the entire DeckRow tree.
//   .hh-deck-stack      — the row holding both <Deck/>s (desktop).
//   .hh-knob-row        — KnobRow's flex container (pitch/tempo/EQ).
//   .hh-effect-rack-row — EffectRack's 3-slot row.
//   .hh-library-drawer  — Library / Sessions outer wrapper.

export const RESPONSIVE_CSS = `
.hh-responsive-root {
  --hh-knob-scale: 1;
}
@media (max-width: 767px) {
  .hh-responsive-root {
    --hh-knob-scale: 0.5;
  }
  .hh-responsive-root .hh-knob-row {
    flex-wrap: wrap;
    gap: 4px !important;
  }
  .hh-responsive-root .hh-knob-row > * {
    transform: scale(var(--hh-knob-scale));
    transform-origin: top left;
    margin: -16px -20px -28px -16px;
  }
  .hh-responsive-root .hh-effect-rack-row {
    flex-direction: column !important;
  }
  .hh-responsive-root .hh-library-drawer {
    position: fixed !important;
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 50;
    max-height: 60vh !important;
    border-top: 2px solid #2c4361 !important;
    box-shadow: 0 -4px 16px rgba(0, 0, 0, 0.6);
  }
  .hh-responsive-root .hh-library-drawer[data-open="false"] {
    transform: translateY(100%);
    pointer-events: none;
  }
  .hh-responsive-root button,
  .hh-responsive-root [role="button"] {
    min-height: 44px;
    min-width: 44px;
  }
}
@media (min-width: 768px) and (max-width: 1023px) {
  .hh-responsive-root .hh-deck-stack {
    flex-direction: column !important;
  }
  .hh-responsive-root .hh-knob-row {
    gap: 8px !important;
  }
}
`;
