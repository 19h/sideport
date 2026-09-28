# Sideport handover

Snapshot: 2026-09-28 (Europe/Berlin), on `master` after the completion pass that followed the
profile-validation resume point. The objective remains the complete Rust implementation of
recovered Sideloadly 0.60 client behavior with a GPUI desktop interface.
[ARCHITECTURE.md](ARCHITECTURE.md) is the requirement ledger; this document records the resume
point, the scope decisions taken, and the verification that still needs the maintainer.
Recovered client behavior is not proof that an external service still operates; its current
availability is unknown.

## State by workstream

| Workstream | Implemented, with evidence | Open |
|---|---|---|
| Account/session | GSA SRP sign-in with 2FA prompts; sessions, remembered passwords and the signing key in the keychain (or a 0600 file); restart restore; recovered `sessions.json` import; 1100 renewal; AOSKit local anisette with per-job fallback to the alternate provider (AOSKit refused requests on this Mac, -45070); default-team API. Fixtures and independent vectors ([APPLE.md](APPLE.md)). | Live sign-in. Mail plug-in anisette and legacy IDMS are not implemented (scope decisions below). |
| Key/certificates, portal provisioning | Durable key and machine UUID, CSR, reuse by public key, confirmed 7460 revocation; device registration, App IDs with free quota, recovered bundle-ID policy, tvOS and per-extension options, trust-verified profiles. Stateful fake portal. | Live portal confirmation. |
| Signing | Child-before-parent signing in original/unsigned/ad-hoc/Apple ID modes; profile preflight with trust; Apple `codesign`/OpenSSL interoperability; cancellation polled per resource, 128 KiB and page (1.4–4.4 ms measured); jobs fail when an input changes during them. | Stock-device acceptance. |
| Device transport/install | usbmuxd discovery/watch (USB and Wi-Fi), lockdown, pairing, resumable AFC staging, installation proxy, recovered retry policy, ZIP streaming with backpressure; device jobs record installations; the reconnect prompt names the device and is withdrawn when the device returns. Fault-injection fixtures; read-only USB probe. | Physical installation; Wi-Fi install; tvOS PIN pairing (no `idevice` CU-pairing API). |
| Device utilities | Apps, profiles, pairing, syslog (real device read); Developer Disk Images (recovered mirrors, cache, `major.minor` fallback, already-mounted check before any download, legacy and personalized iOS 17+ with a TSS client); JIT over the recovered lockdown debugserver; pairing repair; heartbeat; notifications; CLI and desktop controls; demo simulations. Fake device and wiremock. | Physical DDI mount, JIT and pairing repair; iOS 17+ RSD debugserver (beyond the recovered client). |
| Refresh, daemon, IPC | SQLite state; refresh replay; scheduler with cross-process claims, crash takeover after 1 h and future-dated claims after a clock change (store fixtures and a killed-process test); autostart LaunchAgent/XDG entry; `sideport-tray` menu-bar daemon with the recovered labels and actions; the recovered local IPC (`/raise`, `/restart`, `/enqueue`, `/tokens`, `/poll`) on 127.0.0.1:28811 with a loopback token; single-instance hand-over; settings shared across processes. | Opening the tray menu by pointer. |
| Acquisition | `sideloadly:` links, resumable downloads, hash checks, flipped storage, `EnrichIpa`; links as CLI/desktop sources; the packaged app registers the `sideloadly` scheme and IPA documents. | Live link sources. The App Store client is excluded (below). |
| Private services | `sl-services`: BSDIFF40 bspatch, a configurable update client, an RS256 feature-token verifier; the IPC `/tokens` route delivers a token that is verified end to end. No endpoints or keys configured; options are not gated ([SERVICES.md](SERVICES.md)). | Operator-supplied endpoints and key; live checks. |
| Apple Silicon | This Mac as a device, provisioning, recovered wrapper conversion and placement. | Launch verification with an Apple-issued identity; Mac-specific entitlement adjustments. |
| Bundle features | `.deb`/`ar` injection, URL and special sources, substrate rewrites, loose-PNG icon replacement, the filename-mangling utility, folder output ([BUNDLE.md](BUNDLE.md)). | Real tweak packages, live special hosts, native execution of injected binaries on a device. |
| Throughput/consistency | Bounded job events (4096 lossy backlog; stages, facts and prompts always delivered); prompt withdrawal; input change detection; cross-process settings; cancellation latency; one throughput/peak-RSS measurement (466 MiB app: 9.3 s, 27.7 MB, [ENGINE.md](ENGINE.md)). | Peak memory for large Mach-O binaries, which signing holds whole. |
| GPUI/CLI | CLI for every engine workflow; desktop App, Accounts, Devices, Installations and Settings sections covering the workflows above ([UI.md](UI.md)). | Native look at the newer sections (screen capture is not available in this environment); full accessibility; desktop on Linux/Windows. |
| Platforms | macOS (Apple Silicon) development and tests. Non-desktop suite on x86_64 Linux without network access (`scripts/test-linux.sh`). Engine, CLI and tray cross-build for Windows (GNU). | Windows runtime; desktop app on Linux/Windows. |

Broader malformed-format and real-input coverage remains open for Mach-O, CMS, archives and
CgBI PNGs; generated fixtures, Apple `codesign`, OpenSSL and archive readers establish their tested
cases only.

## Scope decisions

- **App Store client, kbsync and FairPlay downloads: excluded.** The recovered Store client
  impersonates Apple's iTunes client, authenticates with kbsync client-attestation tokens produced
  through the Mail plug-in, and downloads FairPlay-protected packages with their decryption
  metadata. Sideport refuses App Store deeplinks as unsupported ([ACQUIRE.md](ACQUIRE.md)).
- **Mail/AltServer plug-in anisette, anisette mode 1 and legacy IDMS: not implemented.** Both
  implementation attempts in this pass were declined before any code was written, and the work was
  not pursued another way. The recovered client itself refuses the plug-in on macOS 14 and later
  (this Mac runs macOS 27); recovered IDMS sessions are reported as skipped on import. Any further
  work here needs a maintainer decision on scope.
- **Private services are modelled, not contacted.** No endpoint, key or claim name was taken from
  the Sideloadly binary, and Sideport does not gate options on feature state.
- **Kept as documented deviations:** filename mangling is a tested utility that unpack/pack do not
  use (names stay original end to end); `Assets.car` icons are not rewritten (refused, as
  recovered); tvOS PIN pairing and the iOS 17+ RSD debugserver are not implemented.

## Verification that needs the maintainer

These steps change a physical device or use a real Apple account, so none was attempted:

1. An Apple ID install onto the attached iPhone with an authorized account, then a refresh.
2. Mounting a Developer Disk Image, enabling JIT and repairing pairing on that iPhone.
3. Live Apple ID sign-in and provisioning, with a dated record.
4. Live `sideloadly:` link and special-source hosts.
5. A native look at the Devices, Settings and editor additions, and opening the tray menu.

## Gates at this snapshot

- `scripts/cargo-ui.sh test --workspace`: 325 passed, 0 failed, 6 ignored. Per crate: sl-macho 10,
  sl-codesign 41, sl-bundle 49, sl-apple 35, sl-device 28, sl-acquire 20, sl-services 20,
  sl-macos 1, sl-engine 73, sl-cli 10, sl-tray 2, sl-app 36.
- `scripts/test-linux.sh`: 279 passed, 0 failed, 3 ignored (x86_64 Debian, no network).
- Strict Clippy (`--workspace --all-targets -D warnings`), `fmt --check` and `git diff --check`
  pass. Clippy reports only a future-incompatibility note for third-party `block` and
  `proc-macro-error2`.

## Dependency order

1. Persist account/session/key state and establish certificate ownership across restarts. Done.
2. Complete team/device/App ID/profile policy and connect it to identity signing. Done with
   fixtures.
3. Implement device transport, streaming upload and installation. Done with fixtures; physical
   acceptance open.
4. Expose account/sign/install in GPUI and CLI; integrate installation state and refresh. Done.
5. Complete acquisition channels, Apple Silicon conversion, device utilities and private
   integrations. Done within the scope decisions above; external availability unverified.

Every architecture-ledger row remains in scope. A row requires implementation and its
fixture/native/live evidence before its completion status changes.

All code changes follow the repository-wide layout contract in [AGENTS.md](../AGENTS.md) and
[STYLE.md](STYLE.md): separate logical stages visibly, group related encoding operations, and
review readability after automatic formatting.

## Assumption register

| ID | Assumption; dependent result | Stress test / falsification probe |
|---|---|---|
| H1 | Recovered artifacts reflect Sideloadly 0.60 behavior; parity claims depend on this. | Compare [reconstruction](../../SIDELOADLY_DECONSTRUCTED.md), [notes](../../notes/AUTH_NOTES.md) and recovered control flow; literals read from the installed binary by address (IPC and tray strings) agree with the decompiled control flow. |
| H2 | Recovered Apple/private endpoints may still accept the protocol; live-workflow claims depend on this. Current state: **unknown**. | Controlled fixtures followed by authorized, dated live requests. |
| H3 | Decoded profile fields represent authentic Apple-issued contents; trust claims depend on this. | Generated tamper/policy fixtures and three local Apple profiles pass; revocation and on-device policy are not checked. |
| H4 | Prefix, wildcard and UDID handling cover target profiles; selection/device-match claims depend on this. | Boundary fixtures and real samples; compare [TN2318](https://developer.apple.com/library/archive/technotes/tn2318/) and [TN3125](https://developer.apple.com/documentation/technotes/tn3125-inside-code-signing-provisioning-profiles). |
| H5 | Inputs and external replacements remain stable during a job, or a change is detected; fidelity claims depend on this. | Identity snapshots fail jobs on same-size rewrites, additions, removals and symlink retargets (fixtures and a mid-job rewrite). A writer restoring both file times is not detected. |
| H6 | Generated fixtures predict corresponding real-input behavior; signing/format claims depend on this. | Independent real IPA/profile samples, native tamper checks and physical installation. |
| H7 | Processes sharing a data directory coordinate through the database lock, and front ends tolerate lossy log lines; consistency/memory claims depend on this. | Concurrent `sideport` processes, two engines on one directory, a killed claimant, clock shifts, and a stalled event receiver are tested. |

Additional component assumptions and probes are in [APPLE.md](APPLE.md), [BUNDLE.md](BUNDLE.md),
[ENGINE.md](ENGINE.md), [DEVICE.md](DEVICE.md), [SERVICES.md](SERVICES.md) and [UI.md](UI.md).

## Bounded observations

- **High impact:** No physical-device installation or live Apple account has been exercised;
  fixture and native-signature evidence does not establish stock-device acceptance.
- **High impact:** Mock portal and private-service success cannot establish current external
  availability; that status remains unknown until a dated live check.
- **Medium impact:** Child-profile selection, legacy prefixes and target-device matching can fail
  independently of a valid main-app signature.
- **Medium impact:** Signing holds each Mach-O binary whole, so peak memory follows the largest
  binary; resources are streamed.
- **Low impact:** Test counts and grouped layout can drift after edits; derive counts from current
  test output and inspect source after formatting.

## Handover quality gates

QG1: This document reports implementation facts and verification status without a normative
judgment. QG2: H1–H7 include dependent results and probes. QG3: Every architecture-ledger
workstream is accounted for, including the excluded and not-implemented parts; unknown service
behavior remains marked unknown. QG4: Numerical results (latency, throughput, memory) are stated
with their conditions in the component documents. QG5: Unperformed device and account
verification is explicit. QG6: Local source/tests, recovered artifacts and linked Apple documents
define provenance. QG7: High/medium/low observations are recorded. These are handover checks; the
**project** completion gates remain open wherever the ledger requires live or physical evidence.
