# ADR-008 — Code signing & auto-updater

| | |
|---|---|
| **Status**     | Proposed |
| **Date**       | 2026-05-18 |
| **Supersedes** | none |
| **Related**    | ADR-001 (stack choice — Tauri shell), PR #39 (Tauri shell scaffold) |

## Context

PR #39 shipped the Tauri desktop scaffold (engine sidecar + bridge token) but
deliberately deferred two production-readiness items:

1. **Code signing** — without it, macOS Gatekeeper shows *"app is damaged"*
   and Windows SmartScreen blocks the installer; users have to right-click /
   click-through to even open the binary.
2. **Auto-updater** — every user-facing critical fix would otherwise require
   manual re-download of the latest installer, which is unacceptable for a
   live-performance tool where the user *cannot* re-download mid-set.

This ADR locks in the design before we wire CI secrets and ship a signed
release. No keys exist yet; this is the framework that the first signed
release will fill in.

## Decision

### Updater mechanism — Tauri built-in

Use Tauri v2's first-party updater plugin. The app:

1. On launch (config-gated; opt-in), fetches a signed JSON manifest from
   `https://hypehouse.live/releases/<channel>/manifest.json`.
2. Verifies the manifest's Ed25519 signature against the public key embedded
   at build time.
3. If `manifest.version > current.version`, emits a `updater://available`
   Tauri event.
4. UI catches the event and shows a non-blocking "Update available — install
   on next restart?" toast. *No silent install during a live set.*
5. If user accepts, downloads the platform-specific bundle, verifies its
   detached signature, emits `updater://downloaded`, and triggers a restart
   into the new binary.

We *reject* third-party updater libraries (`omaha`, `sparkle-rs`) for the
v0.1 surface — Tauri's updater is signature-verified by default, cross-platform,
and ships in the same crate as the rest of the shell. Lower surface area =
lower risk in a hot-path startup module.

### Channels

| Channel | Audience | Manifest URL |
|---|---|---|
| `stable` | default for end users | `releases/stable/manifest.json` |
| `beta`   | opt-in via settings   | `releases/beta/manifest.json`   |

User flips channel in Settings → Updates. Switching channels does NOT auto-
upgrade; the next manifest poll picks up the new version on its normal cadence.

### Key custody

| Key | Where stored | Where used | Rotation |
|---|---|---|---|
| **Updater Ed25519 private key** | 1Password vault `hypehouse-prod` | GH Actions secret `TAURI_UPDATER_PRIVATE_KEY` + passphrase `TAURI_UPDATER_KEY_PASSWORD`. Used to sign each release manifest. | Yearly, with overlap (old key honoured for 30 days post-rotation). |
| **Updater Ed25519 public key**  | Embedded in `tauri.conf.json` `updater.pubkey` at build time | Verifies manifests on every client. | Bumping the public key requires a coordinated release — old clients won't trust new signatures. |
| **macOS Developer ID Application cert** | 1Password — exported as p12, base64'd | GH secret `APPLE_DEVELOPER_ID_CERT_BASE64` + `APPLE_DEVELOPER_ID_PASSWORD`. | Apple sets validity (5 years). |
| **macOS notarisation creds**    | Apple ID + team ID + app-specific password in 1Password | GH secrets `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_PASSWORD`. | App-specific password rotated yearly. |
| **Windows Authenticode cert (EV preferred)** | Hardware token (EV) OR p12 in 1Password (non-EV). | GH secret `WINDOWS_CERT_BASE64` + `WINDOWS_CERT_PASSWORD`. | Vendor sets validity (typically 1-3 years). |
| **Linux GPG key (AppImage / deb)** | 1Password (deferred to v0.2 — see Open questions). | GH secret `GPG_PRIVATE_KEY` + `GPG_PASSPHRASE`. | Yearly. |

Never check any of these into the repo. Generated via:

```bash
# Updater signing keypair — produces a private key (encrypted with password)
# and a public key. Paste public into tauri.conf.json, store private in 1Password.
cargo tauri signer generate -w ~/.tauri/hypehouse-updater.key
```

### Signing & notarisation per OS

**macOS**

1. CI imports the Developer ID Application cert into a temporary keychain
   from the base64-encoded p12.
2. `cargo tauri build --target universal-apple-darwin` signs the .app bundle
   automatically (Tauri reads `bundle.macOS.signingIdentity`).
3. The resulting .dmg is submitted to Apple via `notarytool submit
   --apple-id ... --team-id ... --password ... --wait`.
4. `xcrun stapler staple` attaches the notarisation ticket so offline users
   can still verify.
5. .dmg + .app uploaded as release artifacts.

**Windows**

1. CI imports the Authenticode cert from base64 (or the workflow runner
   has an HSM token via a self-hosted runner for EV certs — out of scope
   for first release; we start with the p12 path).
2. `cargo tauri build` invokes `signtool.exe` automatically when
   `bundle.windows.certificateThumbprint` resolves (or via env var
   `TAURI_SIGNING_PASSWORD`).
3. SmartScreen reputation builds over time — early downloads will still
   prompt; this is unavoidable without an EV cert.

**Linux**

1. AppImage + .deb produced as before (already working in tauri-build.yml).
2. AppImage GPG-signed via `appimagetool --sign`.
3. .deb signed via `dpkg-sig`.
4. v0.2 ships a Flatpak bundle (Flathub submission pipeline is a separate
   project; we defer).

## Consequences

### Positive

- One signed channel + one beta channel covers every distribution case for
  v0.1 / v0.2.
- Signature verification is baked into Tauri's updater — we don't roll our
  own crypto.
- Failed updates surface as Tauri events the UI can present cleanly.
- Auto-updater is opt-in per user → no surprise installs mid-set.

### Negative

- Apple Developer Program: $99/year. Authenticode cert: $150-$500/year.
  These are real, recurring costs and need a budget line item.
- First signed release requires a coordinated cert-procurement step that
  cannot be parallelised with shipping code.
- Updater bugs are uniquely scary: a bad manifest signature could brick
  every install. Mitigated by staged rollouts and the 30-day key overlap
  policy.

### Neutral

- Auto-updater dialog must be very clearly labelled as an opt-in. Users
  in live sets should *never* see a forced-restart dialog. The UI sticks
  to a toast that the user dismisses on their own schedule.

## Implementation plan

This PR ships only the **scaffold** — no real keys, no live release:

1. `tauri.conf.json` adds an `updater` block with placeholder pubkey + dialog disabled.
2. `tauri/src/updater.rs` new module wrapping Tauri's updater builder.
3. `tauri/src/commands.rs` adds `check_for_updates` + `install_pending_update`.
4. `.github/workflows/release.yml` new pipeline keyed off `v*` tags,
   with all the cert/notarisation steps stubbed behind secrets that don't
   exist yet (workflow no-ops gracefully).
5. `docs/RELEASE.md` documents how to obtain certs + populate secrets.

A follow-up PR will:

- Replace placeholder pubkey with a real one.
- Add the manifest publishing step (S3 / GitHub Releases mirror).
- Wire the UI toast subscriber.
- First signed release.

## Open questions

- **Flatpak / snap distribution** — deferred to v0.2. Flathub submission is
  its own multi-week process.
- **Delta updates** — Tauri 2 supports them via the differential bundle
  format. Defer; full-bundle download is < 30 MB which is acceptable for v0.1.
- **EV Authenticode cert on a self-hosted runner** — defer until SmartScreen
  reputation becomes a real complaint. Standard Authenticode is enough to
  remove the "unknown publisher" red flag.
- **Linux desktop integration** — `.desktop` file + MIME associations land
  in v0.2 alongside Flatpak.
