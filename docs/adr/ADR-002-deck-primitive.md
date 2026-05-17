# ADR-002 — Deck primitive: 2 decks + AI co-pilot mode

**Status**: Accepted 2026-05-17
**Decider**: Sandeep Gorai

## Context

Number of decks + the relationship between manual control and AI auto-mixing is the central UX decision. Three options were considered:

| Option | Decks | Co-pilot |
|---|---|---|
| Classic 2-deck DJ | 2 + xfader | No |
| Serato/CDJ-3000 tier | 4 + xfader + sampler | No |
| **2-deck + AI co-pilot** | **2 + xfader** | **Yes, per-deck toggle** |

## Decision

2 decks (A, B) + master crossfader + AI co-pilot mode that can be toggled per deck or for the whole session. The co-pilot:

1. Picks the next track from the library when the active track has <30s remaining.
2. Drops the picked track onto the inactive deck.
3. Executes a phrase-aligned transition (16/32-bar boundary, BPM-matched, key-compatible, LUFS-matched) at the next drop or breakdown.
4. The user can intervene at any moment — pressing any control on the inactive deck or any MIDI control flips the deck to manual until the user toggles co-pilot back.

## Why

- **2 decks is the industry contract**. Pioneer DDJ-200, NI Traktor S2, Serato Lite all ship with 2 decks. Anyone with hardware can plug in and play day 1.
- **Co-pilot is HypeHouse's actual differentiator**. v1 demonstrated the AI can produce coherent mixtapes; v2 brings that strength into a live context as an opt-in feature. Manual purists can ignore it.
- **2-deck implementation cost is half of 4-deck** for the state machine, EQ, loop, hot cue logic. Faster to ship.
- **Hybrid mode** (manual on A, AI on B) is the unique experience: party host can keep authoritative control of incoming tracks while AI handles the "outro and slot in something compatible" busywork.

## Rejected alternatives

### 4 decks + sampler

Higher ceiling but doubles the deck-state surface area + needs more complex crossfader logic (3D vs 1D). Re-evaluate post-launch if user demand surfaces.

### 2 decks no co-pilot

Wastes the v1 audio-AI investment. Just becomes "yet another DJ app".

## Consequences

- Deck state struct in Rust has 2 instances, indexed `[DeckA, DeckB]`. Add `[DeckC, DeckD]` later requires only state array growth + 2 more UI panels; no architectural change.
- Co-pilot toggle is per-deck. UI shows a tiny `AI` LED on each deck. MIDI mapping reserves a button for toggle.
- Take-over rule: any deck-level MIDI/UI input on a co-pilot-controlled deck → flip that deck back to manual, AI completes the in-progress transition cleanly then stands down.
- Hot cues: 8 per deck. Loop: in/out + 1/2/4/8/16-bar auto-loop. Industry standard.

## Open implementation questions

- Co-pilot's decision latency — RPC roundtrip to Python service. Target <500ms for next-track decision; transition execution is engine-local.
- "Drop-in / take-over with no audio glitch" — needs a documented handoff protocol in ADR-003.
