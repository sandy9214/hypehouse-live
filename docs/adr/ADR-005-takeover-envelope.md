# ADR-005 — Takeover envelope

**Status**: Accepted 2026-05-17
**Decider**: Sandeep Gorai
**Trigger**: Council review flagged ADR-002's "user can take over anytime" as under-specified (Codex + gpt-4o-mini).

## Context

ADR-002 introduced AI co-pilot per-deck with the rule "user can take over anytime". What that means in practice during an in-flight AI-driven transition is ambiguous: hard cancel? Snap-to-manual? Bounded envelope handoff? Each choice has different audible artifacts.

## Decision

**Bounded handoff envelope** of 1 bar (or the remainder of the current bar, whichever is shorter). The audio thread keeps applying the AI's in-flight automation (crossfader, EQ, pitch) for that bounded window while the user's manual controls start influencing the same parameters via a cross-fade between AI-target and user-current. After the envelope ends, the deck is fully manual and the AI has no further authority until the user re-engages.

```
   t = takeover event arrives
   ┌──────────────────────────────────────┐
   │ AI authority │ Handoff │ Manual auth │
   │              │ envelope│             │
   └──────────────┴─────────┴─────────────┘
   t-1            t         t+1 bar (max)
```

## Rationale

- **Hard cancel** at t = audible parameter jump. Crossfader jumps to user's last position, EQ snaps, BPM-match drops. Avoid.
- **Snap-to-manual** at t = same problem.
- **Unbounded envelope** = user feels disempowered ("I'm taking over but AI is still doing things").
- **Bounded 1-bar envelope** = imperceptible to listeners (bar-aligned crossfade), gives user instant authority, AI cleanly stands down. Industry standard for auto-DJ apps (Algoriddim djay, Engine DJ).

## Why 1 bar and not 1 beat / 4 bars

- 1 beat = audible: parameter ramps are too short to mask, listener hears a "click".
- 4 bars = too long: user pressed a button 4 bars ago and the AI is still touching their crossfader.
- 1 bar = sweet spot. ~2 seconds at 120 BPM. Listener hears it as the natural end of a transition phrase.

## Event sequence

```
Event(t):    TakeOver { deck }
              │
              ├─ Control thread: mark deck.copilot_engaged = false
              │  Mark deck.handoff_until_frame = current_frame + 1_bar_frames
              │
              └─ Audio thread: continues AI's last-emitted automation envelopes
                  until handoff_until_frame; user inputs from t onwards
                  cross-fade against the AI's envelope via a 1-bar linear ramp.
                  After handoff_until_frame, user is sole authority.
```

## Edge cases

- **Track ends during handoff**: AI's transition was mid-execution; aborting it leaves silence. Engine completes the AI's last-planned action (typically: bring crossfader to the user's deck) but still hands authority to user — no new AI commands accepted.
- **User triggers another co-pilot event during handoff**: ignored. Co-pilot button is debounced for the handoff duration.
- **No active transition (deck just sitting)**: handoff envelope is effectively no-op — user controls are immediately authoritative.

## Open questions

- Does the handoff also abort any pending track-load on the AI's chosen next track? Default: no. The buffer is decoded but the AI doesn't get to start playing it on the user's manual deck. The user can choose to play it (the track stays in the cue lane) or discard.
- Should we expose a UI indicator for "handoff in progress"? Yes — a thin LED on the deck panel that fades in/out over the bar.
