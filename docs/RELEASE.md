# Release process — signed Tauri bundles (ADR-008)

This document walks through everything a maintainer must do **once** to
ship the first signed HypeHouse Live release. The CI workflow at
`.github/workflows/release.yml` is the automation; this doc is the
human-side checklist.

> **Status (2026-05-18):** scaffold only. Every step below is documented
> ahead-of-time. No keys exist yet. Until the secrets listed in
> §"GitHub Actions secrets" are populated, the release workflow runs to
> completion but emits **unsigned** bundles + workflow warnings. Do not
> promote unsigned drafts to public releases.

## 1. Prereqs — accounts & costs

| What | Where | Cost / cadence |
|---|---|---|
| Apple Developer Program       | https://developer.apple.com/programs/ | $99 / year |
| Authenticode code-signing cert (non-EV) | DigiCert / Sectigo / SSL.com | $150-$300 / year |
| Authenticode EV cert (optional, removes SmartScreen prompt) | DigiCert / Sectigo | $300-$500 / year, requires HSM token |
| `hypehouse.live` DNS + HTTPS  | already provisioned | — |
| 1Password vault `hypehouse-prod` | already provisioned | — |

Linux signing (GPG) is free but **deferred to v0.2** — see ADR-008
Open Questions.

## 2. Generate the Tauri updater signing keypair

The updater verifies the manifest's Ed25519 signature on every client.
The private key signs; the public key ships embedded in the app.

```bash
# Install the Tauri CLI matching the v2 crate.
cargo install tauri-cli@2 --locked

# Generate keypair. You'll be prompted for a passphrase — use a strong
# random one and stash it in 1Password as `tauri-updater-key-password`.
cargo tauri signer generate -w ~/.tauri/hypehouse-updater.key

# Output:
#   * `~/.tauri/hypehouse-updater.key` — encrypted private key
#   * `~/.tauri/hypehouse-updater.key.pub` — base64 public key (one line)
```

Then:

1. Paste the **public key** into `tauri/tauri.conf.json` →
   `plugins.updater.pubkey`. Commit this; it's safe to share.
2. Upload the **private key file + passphrase** to 1Password under
   `hypehouse-prod` → `tauri-updater-key`.
3. Set `plugins.updater.active` to `true` (currently `false` in the
   scaffold).

## 3. Obtain code-signing certificates

### macOS — Developer ID Application

1. Log into https://developer.apple.com/account.
2. Certificates → "+" → **Developer ID Application** → submit a CSR
   generated from Keychain Access on a Mac.
3. Download the `.cer`, double-click to install into Keychain.
4. Export from Keychain as `.p12`, choose a strong password.
5. `base64 -i HypeHouseLive.p12 -o HypeHouseLive.p12.b64`
6. Store the `.p12` + password in 1Password under `apple-developer-id`.

Also create an **app-specific password** for notarytool:

1. https://account.apple.com → Sign-In and Security → App-Specific Passwords.
2. Label it `hypehouse-notarytool`. Store in 1Password.

### Windows — Authenticode

1. Buy a non-EV Authenticode cert from DigiCert / Sectigo / SSL.com.
2. Vendor will issue a `.pfx` or `.p12` (CSR-flow varies by vendor).
3. `base64 -w0 hypehouse-codesigning.pfx > hypehouse-codesigning.pfx.b64`
4. Store the `.pfx` + password in 1Password under `windows-authenticode`.

EV certs require a hardware HSM token and a self-hosted GitHub runner —
skip until SmartScreen reputation becomes a real complaint.

### Linux — deferred

GPG-signed AppImage + .deb land in v0.2. The release workflow includes
stubs but does not yet block on them.

## 4. GitHub Actions secrets

Add these in `Settings → Secrets and variables → Actions → New repository secret`.
Every name is exact; the workflow's `env:` block references them verbatim.

| Secret name | Value | Used by |
|---|---|---|
| `TAURI_UPDATER_PRIVATE_KEY` | full contents of `~/.tauri/hypehouse-updater.key` | All OSes — signs the updater manifest |
| `TAURI_UPDATER_KEY_PASSWORD` | passphrase chosen in step 2 | All OSes |
| `APPLE_DEVELOPER_ID_CERT_BASE64` | contents of `HypeHouseLive.p12.b64` | macOS only |
| `APPLE_DEVELOPER_ID_PASSWORD` | `.p12` export password | macOS only |
| `APPLE_SIGNING_IDENTITY` | exact identity string, e.g. `Developer ID Application: HypeHouse Live (TEAMID)` | macOS only |
| `APPLE_ID` | Apple ID email | macOS notarisation |
| `APPLE_TEAM_ID` | 10-char team ID (find at developer.apple.com → Membership) | macOS notarisation |
| `APPLE_PASSWORD` | app-specific password from step 3 | macOS notarisation |
| `WINDOWS_CERT_BASE64` | contents of `hypehouse-codesigning.pfx.b64` | Windows only |
| `WINDOWS_CERT_PASSWORD` | `.pfx` export password | Windows only |
| `GPG_PRIVATE_KEY` | (deferred, v0.2) | Linux AppImage signing |
| `GPG_PASSPHRASE` | (deferred, v0.2) | Linux AppImage signing |

`gh secret set` works for each:

```bash
gh secret set TAURI_UPDATER_PRIVATE_KEY < ~/.tauri/hypehouse-updater.key
gh secret set APPLE_DEVELOPER_ID_CERT_BASE64 < HypeHouseLive.p12.b64
# … etc.
```

## 5. Cutting a release

```bash
# 1. Make sure the working tree is clean + main is green.
git checkout main && git pull && git status

# 2. Bump the version. Three places must match:
#    - tauri/tauri.conf.json → "version"
#    - tauri/Cargo.toml → version
#    - engine/Cargo.toml → version  (kept in lock-step for now)
$EDITOR tauri/tauri.conf.json tauri/Cargo.toml engine/Cargo.toml

# 3. Commit the bump.
git commit -am "chore: bump to v0.2.0"
git push

# 4. Tag + push the tag. This is what fires release.yml.
git tag v0.2.0
git push origin v0.2.0

# 5. Watch the workflow.
gh run watch
```

Output: a **draft** GitHub Release at
`https://github.com/sandy9214/hypehouse-live/releases` with signed
artifacts attached. *Do not click Publish until step 6 passes.*

## 6. Manually verify each signed artifact

```bash
# macOS — staple + Gatekeeper check
spctl -a -t exec -vvv HypeHouseLive.app
xcrun stapler validate HypeHouseLive.app

# Windows — signtool verify
signtool verify /pa /v HypeHouseLive.msi

# Updater manifest — make sure the .sig matches the .tar.gz / .zip
# Tauri's CLI bundles the signature alongside the artifact; check
# it parses + verifies with the public key in tauri.conf.json.
```

If any of those fail, the release stays in draft and the secrets path
is broken — re-check step 4.

## 7. Publish + update the manifest server

1. Click **Publish release** in the GitHub UI.
2. Run the (TODO — separate PR) `scripts/publish-manifest.sh` to
   upload the manifest to `https://hypehouse.live/releases/<channel>/`.
3. Smoke-test with an existing install: launch HypeHouse Live, wait
   for the `updater://available` event, click "install + restart".

## Key rotation

- **Apple cert:** Apple sets the validity (~5 years). Calendar-set a
  reminder for 11 months before expiry. Process: generate new cert,
  add as `APPLE_DEVELOPER_ID_CERT_BASE64_NEW`, swap workflow env,
  remove old secret.
- **Authenticode cert:** vendor-dependent (1-3 years). Same swap path.
- **Updater signing key:** rotate yearly. The clients only honour ONE
  pubkey at a time, so the rotation is a two-release operation —
  ship a release signed with both old + new keys, wait 30 days, then
  ship one signed with new key only. (Tauri 2's bundle format supports
  multiple signatures; the integration PR will exercise this path.)

## When the workflow runs WITHOUT secrets

The workflow is intentionally tolerant — every signing step is guarded
behind `if: env.SECRET_NAME != ''`. With no secrets set, it builds
unsigned bundles + uploads them as draft artifacts + prints loud
`::warning ::` lines. This lets us validate the *plumbing* before
the certs land. **Do not promote those drafts.**
