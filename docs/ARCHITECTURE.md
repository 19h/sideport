# Sideport — architecture

Sideport is a from-scratch Rust reimplementation of the *developer sideloading workflow* documented in
`../SIDELOADLY_DECONSTRUCTED.md` and `../notes/`: take an `.ipa` you are entitled to install, provision it
with **your own** Apple ID (free or paid developer account), re-sign it and install it on **your own**
device, then keep it refreshed before the free-profile expiry.

## Scope

In scope (parity with the documented workflow):

| Area | What |
|---|---|
| Apple ID | GrandSlam SRP-6a login, trusted-device / SMS 2FA, Xcode app-token, session persistence |
| Anisette | macOS local provider (AOSKit, same as the Mac's own provisioning); user-configured remote (v1 JSON) server |
| Developer portal | teams, devices, certificates (CSR, reuse, revoke-oldest on limit), app IDs, team provisioning profiles, free/paid detection |
| Bundle | IPA unpack, Info.plist edits (name, version, bundle id policy, min OS, device-family limits, file sharing, arbitrary keys), extension / watch-app removal, file replacement, app icon replacement, entitlement overrides |
| Code signing | pure-Rust Mach-O signer: SHA-1 + SHA-256 CodeDirectories, requirements, XML + DER entitlements, CMS with Apple CDHash attributes, CodeResources seal, fat binaries, ad-hoc mode |
| Install | usbmuxd discovery (USB + Wi-Fi), lockdown, resumable AFC upload (file or streamed zip), installation_proxy with progress, retry state machine |
| Lifecycle | installations database, expiry tracking, automatic refresh, profile inspection/removal on device |
| Front-ends | gpui desktop app, scriptable CLI |

Deliberately **not** implemented:

* Third-party tweak injection pipeline (Substrate/Substitute downloads, `.deb` unpacking, "spoofer" tweaks) —
  its purpose is modifying/unlocking other people's apps and defeating their integrity checks.
* App Store purchase/download, FairPlay `.sinf` enrichment, kbsync generation, the Mail.app plug-in.
* Anything tied to Sideloadly's commercial backend: feature tokens, paid-feature gating, its private remote
  anisette endpoint, self-update, Patreon OAuth. Sideport has no paywall and talks only to Apple and to
  servers the user configures.
* The legacy IDMS (`clientDAW.cgi`) login, which Apple no longer serves.

## Crates

```
sl-macho     Mach-O / fat parsing and load-command editing                (no I/O, no deps on other crates)
sl-codesign  signature generation, CodeResources, CMS, DER entitlements,    (sl-macho)
             provisioning-profile decoding, signing identities
sl-bundle    IPA unpack (parallel), bundle model, patching, deep signing,   (sl-codesign, sl-macho)
             deterministic (streamable) repacking
sl-apple     anisette providers, GSA SRP auth + 2FA, developerservices2      (independent)
sl-device    usbmuxd/lockdown/AFC/instproxy/misagent via the `idevice` crate (independent)
sl-engine    job model + pipeline, provisioning policy, install retry logic,  (all of the above)
             persistent store (accounts, secrets, keys, settings, installations DB), refresh scheduler
sl-cli       `sideport` command-line front-end                              (sl-engine)
sl-app       gpui desktop app                                               (sl-engine)
```

### Runtime model

* The engine owns a multi-threaded **tokio** runtime (network + device I/O). CPU-heavy work (unzip, hashing,
  signing, compression) runs on **rayon** inside `spawn_blocking`.
* Front-ends never touch tokio directly: `Engine` methods return runtime-agnostic futures/channels
  (`async-channel`, `futures::oneshot`), so gpui's executor can await them.
* A running job emits a stream of `JobEvent`s (log line, stage/progress, info facts, prompt requests,
  completion). Prompts carry a oneshot reply channel; dropping it = cancel.
* Cancellation is cooperative through a `CancellationToken` checked between steps and inside long loops.

### Performance choices

* Unzip uses a memory-mapped archive and extracts entries in parallel.
* CodeResources hashing, per-page CodeDirectory hashing and independent nested bundles are signed in parallel;
  dependency order is respected (children before parents).
* The signature size is computed exactly from a placeholder build, so each binary is hashed once.
* The output IPA can be streamed straight into the AFC upload (no second copy on disk); the zip is
  deterministic so an interrupted upload resumes by skipping already-uploaded bytes.

### Security choices

* TLS is always verified. Apple's `gsa.apple.com` chains to *Apple Root CA*, which is not in public root
  stores, so that root is embedded and added as an extra trust anchor.
* Session tokens and (optional) saved passwords live in the macOS Keychain; a 0600 file store is the fallback
  on other platforms and in tests. The signing key is written 0600.
* Archive extraction rejects absolute paths, `..` components and symlinks escaping the bundle.

### Testing strategy

* Unit tests per crate with hand-computed vectors (SRP against an in-test server implementation, AES-CBC/GCM
  token decoding, DER entitlements, requirement serialization, CodeDirectory layout).
* `codesign`-verified integration tests on macOS: binaries we sign are checked with `codesign --verify` /
  `codesign -dvvv`, and CMS blobs with `openssl cms -verify`.
* Fixture IPAs are **generated by the tests** (tiny arm64 apps compiled with the local Xcode toolchain, or
  synthetic Mach-Os when no toolchain is present) — no third-party app binaries.
* Mock Apple endpoints (wiremock + an SRP server) drive the full login/provisioning flow; a fake device
  implementation drives the install retry state machine.
