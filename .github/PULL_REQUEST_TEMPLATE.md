<!--
PR template for hypehouse-live. Short. Each section answerable in 2-3 lines.
Council merge gate (cloud-quint APPROVE) is enforced manually until branch protection lands post-v0.1.
-->

## Summary

<!-- 1-3 sentences: what changed, user-visible effect. -->

Closes #<ISSUE_NUMBER>

## Test plan

<!--
How you verified this works. Be specific — commands, fixtures, manual steps.
- [ ] cargo test --all-targets (if engine/)
- [ ] npm test + npm run build (if ui/)
- [ ] pytest (if copilot/)
- [ ] Manual / end-to-end run if cross-process behavior changed
-->

## ADR refs

<!-- Link ADRs this PR implements or amends. See docs/adr/. e.g. ADR-001, ADR-003. -->

## Risk class

<!-- Pick one. P0 = audio-thread / safety / data loss. P1 = capital / live-set break. P2 = degradation. P3 = cosmetic / docs / chore. -->

- [ ] P0 — audio-thread hot path, MIDI input validation, event-log integrity
- [ ] P1 — engine ↔ bridge ↔ UI contract, co-pilot decision flow
- [ ] P2 — visualization, dashboard, non-critical state
- [ ] P3 — docs, tests, CI, chore

## Council review

<!--
HARD MERGE RULE (project-wide): every PR needs /council --cloud-quint APPROVE
(Codex + Gemini Flash + Groq Llama 70B + GitHub-DeepSeek-V3 + Claude).
Auto pr-review action alone is NOT sufficient.
-->

- [ ] `/council --cloud-quint` quorum APPROVE recorded
- [ ] No P1 REQUEST_CHANGES outstanding
