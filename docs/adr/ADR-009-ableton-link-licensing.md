# ADR-009 — Ableton Link licensing (LGPL v3 implications)

**Status**: Proposed 2026-05-18 — pending user sign-off before the v0.2.x Ableton Link real-binding PR lands.
**Decider**: Sandeep Gorai
**Trigger**: ADR-007 §v0.2 scaffold PR landed `clock_sync::link_real::LinkReal` as an `unimplemented!()` placeholder; the real `rust-link` binding pulls Ableton's Link SDK which is LGPL v3. We need an explicit licensing decision before vendoring or linking against it.

## Context

Ableton Link is the de-facto peer-to-peer beat-sync protocol on modern DJ rigs (iPad apps, Maschine, Ableton Live, MPC One, Traktor, rekordbox). Adding Link to hypehouse-live is high user value — covered in ADR-007 §v0.2.

The reference implementation Ableton ships is **C++, LGPL v3** (https://github.com/Ableton/link, `LICENSE.md`). The community Rust binding `rust-link` (https://github.com/anzev/rust-link) wraps it via `bindgen`. The license therefore propagates to anything that **statically links** the binding into our binary.

LGPL v3 has two material constraints on hypehouse-live:

1. **Dynamic linking = OK.** If we load Link as a dynamic library (`.dylib` on macOS, `.so` on Linux, `.dll` on Windows), users can replace it with their own modified version and our license remains MIT.
2. **Static linking = copyleft trigger.** If we statically bake Link into the binary, the LGPL requires we either (a) provide object files so users can re-link against a modified Link, or (b) make the surrounding code LGPL-compatible. The first path is operationally painful for the auto-updater (ADR-008); the second is incompatible with our current MIT license on the engine.

## Decision (proposed)

**Use dynamic linking via the community `rust-link` crate.** The crate exposes a `cargo` feature `dynamic` (or equivalent — to be verified during the v0.2.x PR) that loads `libabletonlink.dylib` / `.so` / `.dll` at runtime instead of statically baking it.

**Action items before the v0.2.x PR lands:**

1. **User sign-off on this ADR.** Explicit "yes, ship Link as a dynamic-link LGPL dep" before the v0.2.x PR is opened. The MERGE GATE rule requires it.
2. **Bundle the Link `.dylib` separately in the macOS .app bundle** (ADR-008 code-signing path), with a `LICENSE-LINK.txt` next to it pointing at https://github.com/Ableton/link/blob/master/LICENSE.md.
3. **Add a `LICENSES/LINK-LGPL-v3.txt`** at the repo root mirroring the upstream LICENSE so anyone auditing our source tree sees the LGPL banner.
4. **CI matrix**: confirm the `rust-link` crate builds on Linux + macOS + Windows. If Windows requires Visual Studio C++ build tools, document it in the README.
5. **Pin the `rust-link` crate version** to a specific commit / tagged release; LGPL upstream changes can affect our obligations.
6. **Auto-updater note (ADR-008)**: when we ship an update, the user's locally-modified Link `.dylib` (if any) must NOT be overwritten. The updater whitelist already covers `Frameworks/` — confirm Link lives there.

## Why dynamic, not static

* MIT-on-engine + LGPL-on-Link via dynamic link is the standard pattern (cf. Audacity's FFmpeg integration). It's the path with the least lawyer-and-auditor friction.
* The Link SDK is ~150 KB compiled — the dynamic-load overhead is negligible.
* Users can swap in a newer Link `.dylib` (e.g. for a bug fix Ableton ships before we cut a release) without rebuilding the engine.

## Alternatives considered

* **Re-implement Link in pure Rust.** Link's wire protocol is documented (UDP multicast + a specific tempo-arbitration algorithm). A clean-room Rust impl would dodge LGPL entirely. **Rejected for now** — the protocol has been incrementally evolved over 8+ years and the interoperability bar is high; a third-party impl that mis-syncs by a millisecond breaks the user's set. Revisit if the LGPL operational burden becomes painful.
* **Skip Ableton Link.** Use MIDI clock OUT (already shipped) + Pioneer ProDJ Link only. **Rejected** — iPad-DJ workflows specifically rely on Link; not shipping it cuts off a major user segment.

## What this ADR does NOT decide

* The exact `rust-link` crate version / fork — pinned during the v0.2.x PR.
* The UI for surfacing peer count + Link toggle — covered in ADR-007 §v0.2 follow-up.
* Whether to expose a Link-master toggle (engine can be the LAN's tempo master, or a follower). Default = follower if any peer is on the LAN, master if alone. Tracked in v0.2.x PR scope.

## References

* Ableton Link upstream: https://github.com/Ableton/link
* Ableton Link LICENSE: https://github.com/Ableton/link/blob/master/LICENSE.md (LGPL v3)
* Community Rust binding `rust-link`: https://crates.io/crates/rust-link (verify license + binding shape during v0.2.x PR)
* LGPL v3 dynamic-vs-static guidance: https://www.gnu.org/licenses/lgpl-3.0.en.html
* ADR-007 §v0.2 — the scaffold PR this ADR unblocks
* ADR-008 — code-signing + auto-updater (bundled `.dylib` lives here)
