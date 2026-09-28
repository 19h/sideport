# Sideport handover

Snapshot: 2026-09-28 (Europe/Berlin), after the profile-validation follow-up. The objective
remains the complete Rust implementation of recovered Sideloadly 0.60 client behavior with a
GPUI desktop interface. [ARCHITECTURE.md](ARCHITECTURE.md) is the requirement ledger; this document
records the current resume point and **known** open work. Recovered client behavior is not
proof that an external service still operates. Its current availability is unknown.

## Completed: profile validation follow-up

The profile-validation resume point is committed. Commit `8b40999` (made under the maintainer's
identity during this session) captured the codesign parser/trust sources; the following commit
adds the bundle preflight, tests and documentation.

| Item | Result |
|---|---|
| Field review | `validate_for` checks `CreationDate <= t < ExpirationDate`, team and team entitlement, `PREFIX.PATTERN`, platform, certificate DER and UDID. Decision: an absent `ApplicationIdentifierPrefix` list accepts only `PREFIX == Team ID`; a listed legacy prefix may differ. Wildcards match a nonempty suffix after one trailing `*`. UDIDs use the recovered casefold/hyphen-removal rule and must be hexadecimal; `ProvisionsAllDevices` is parsed. Fifteen focused tests cover the distinct cases. |
| Trust | `ProvisioningProfile::verify_trust` with `ProfileTrust` verifies one RSA SignerInfo, signed attributes, leaf → issuer → configured anchor, Apple signer/issuer names, CA/key usage and validity at `CreationDate`. It is separate from field validation. Six generated-chain tests plus a local ignored probe: three Xcode-managed Apple profiles verified against the bundled Apple Root CA on 2026-09-28, tampered copies rejected. |
| Signing boundary | `SigningRequest::requirements` (`ProfileRequirements`: device UDID, platform, trust, time). Every profile the pass would embed is checked against its own bundle ID before any file changes. Children without profiles inherit entitlements and embed nothing, as recovered. The engine passes defaults until Apple ID/device jobs exist. |
| Documentation | APPLE.md profile contract, sources, A14/A15; ARCHITECTURE.md ledger/A5; BUNDLE.md and ENGINE.md counts. |
| Gates | `scripts/cargo-ui.sh test --workspace`: 141 passed, 0 failed, 1 ignored (sl-macho 10, sl-codesign 39, sl-bundle 27, sl-apple 33, sl-engine 19, sl-cli 5, sl-app 8, sl-device 0). Strict Clippy, `fmt --check` and `git diff --check` pass. Clippy reports only a future-incompatibility note for third-party `block` and `proc-macro-error2`. |

Fixture and native-signature checks still do not establish stock-device acceptance. A trusted
profile that matches decoded fields can still be refused by a device for reasons not modelled
here (revocation, entitlement policy, device state).

## Open implementation and verification

| Workstream | Open work | Required evidence / source |
|---|---|---|
| Account/session | Done: persisted sessions, remembered passwords, restart restore, recovered `sessions.json` import, 1100 renewal, AOSKit local anisette bridge with per-job fallback (AOSKit refused requests on this Mac with -45070). Open: Mail/AltServer plug-in anisette, kbsync, legacy IDMS, live checks. | Dated live checks. [APPLE.md](APPLE.md), [AUTH_NOTES](../../notes/AUTH_NOTES.md). |
| Key/certificates | Done: durable key and machine UUID, CSR flow, reuse by public key, `is_ours`, confirmed 7460 revocation. Open: live portal confirmation. | Dated live checks. [APPLE.md](APPLE.md), `crates/sl-engine/src/engine/provision.rs`. |
| Portal provisioning | Done against the fake portal: device registration, App ID reuse/creation/quota, bundle-ID policy, tvOS selection, profile download with trust verification, per-extension option. Open: device-target wiring (needs `sl-device`), profile renewal via refresh, live checks. | Live account/device checks. [APPLE.md](APPLE.md), [MOBDEV_NOTES](../../notes/MOBDEV_NOTES.md). |
| Apple ID signing | Done: exports and device installs through provisioning (fixtures); native codesign/OpenSSL checks. Open: physical installation. | Physical installation with an authorized account. [ENGINE.md](ENGINE.md), `crates/sl-engine/src/engine/sideload.rs`. |
| Device transport/install | Done with fixtures: `sl-device` discovery/watch, lockdown, pairing, AFC resumable staging, framed installation proxy, recovered retry policy, ZIP streaming with backpressure, engine device jobs (Apple ID/ad-hoc/original). Read-only USB probe passed. Open: physical installation, Wi-Fi, tvOS PIN pairing/heartbeat. | Physical iOS/tvOS checks with an authorized identity. [DEVICE.md](DEVICE.md). |
| Device utilities | Done: app list/uninstall, profile list/remove, pairing, syslog stream with filter (real device read). Open: pairing repair UI, Developer Disk Images including personalized iOS 17+, JIT, notifications. | Service fixtures and physical checks. [DEVICE.md](DEVICE.md), [MOBDEV_NOTES](../../notes/MOBDEV_NOTES.md). |
| Refresh/state | Done: SQLite accounts/installations/stored files, installation records from device jobs, refresh replay, scheduler with cross-process claims. Open: tray/autostart/LaunchAgent, local IPC, clock-shift/crash tests. | Restart/crash, clock-shift and concurrent-process tests. [ENGINE.md](ENGINE.md), [DEVICE.md](DEVICE.md). |
| Acquisition channels | Done with fixtures: `sideloadly:` links (recovered rules, messages, 155 countries), resumable HTTP downloads with recovered backoff, HTML/ZIP checks, MD5/SHA-1, flipped storage, `EnrichIpa`; links as engine/CLI sources. Open: App Store authentication/purchase/download (needs kbsync from the Mail plug-in, which the recovered client disables since Sonoma), URI-scheme registration. | Controlled transport fixtures and dated live checks. [ACQUIRE.md](ACQUIRE.md), [reconstruction §9](../../SIDELOADLY_DECONSTRUCTED.md). |
| Private services | Done in `sl-services` (docs/SERVICES.md): BSDIFF40 `bspatch` cross-checked against `/usr/bin/bspatch`; a configurable `go-selfupdate` update client (JSON manifest, patch-or-full download, SHA-256 verification, `.new`/`.old`/`.bak` swap with the recovered daemon↔lib restore bug documented and corrected); a generic RS256 feature-token verifier with a Sideport-defined claim schema; engine `ServiceConfig`/`services_status`/`check_update`/`apply_feature_token` and CLI `services status`/`check-update`. Nothing is configured by default; no endpoint, key or claim name is taken from the Sideloadly binary and no private service is contacted. Open: operator-supplied endpoints/key and dated live checks; wiring feature state into provider/option gating; the OAuth HTTP listener lives in the engine IPC server. Server behavior/availability unknown. | Operator-supplied endpoints and dated live checks. [SERVICES.md](SERVICES.md), [reconstruction §13](../../SIDELOADLY_DECONSTRUCTED.md). |
| Apple Silicon | Done with fixtures: Mac as a device (provisioning UDID, computer name), Apple ID provisioning, folder output, recovered wrapper conversion and `/Applications` placement rules. Open: launch verification with an Apple-issued identity; Mac-specific entitlement adjustments. | Native launch verification. [DEVICE.md](DEVICE.md), [reconstruction §8.3](../../SIDELOADLY_DECONSTRUCTED.md). |
| Bundle features | Remote/special/deb/ar injection; asset-catalog icon extraction/editing; portable filename indirection, Unicode/long-name/Windows cases; folder output; identity-signed entitlement overrides. | Format/path/native icon fixtures and cross-platform checks. [BUNDLE.md](BUNDLE.md), [BUNDLE_NOTES](../../notes/BUNDLE_NOTES.md). |
| Throughput/consistency | ZIP-to-AFC streaming with backpressure/resume, bounded event queue, finer cancellation in sealing/signing, input TOCTOU detection, cross-process settings coordination. | Byte-identical ZIP/ZIP64 suffixes, fault injection, peak RSS, throughput and cancellation latency. [BUNDLE.md](BUNDLE.md), [ENGINE.md](ENGINE.md). |
| GPUI/CLI | Account/2FA/team, certificate/App ID/device controls, install/progress/refresh, Store/URI and feature settings. Full accessibility and Linux/Windows runtime remain unverified. Current UI/CLI cover inspection/local export; CLI also checks remote anisette. | Rendered interaction, keyboard/accessibility, native package and account/device flows. [UI.md](UI.md), [ENGINE.md](ENGINE.md). |

Broader malformed-format and real-input coverage remains open for Mach-O, CMS, archives and
CgBI PNGs. Existing generated fixtures, Apple `codesign`, OpenSSL and archive readers establish
their tested cases only. Inspection does not check CRCs of unvisited ZIP payloads. Current
source, test and limits are detailed in [ARCHITECTURE.md](ARCHITECTURE.md),
[BUNDLE.md](BUNDLE.md) and [ENGINE.md](ENGINE.md).

## Dependency order

1. Persist account/session/key state and establish certificate ownership across restarts.
2. Complete team/device/App ID/profile policy and connect it to identity signing, including
   nested bundles and entitlement authorization.
3. Implement device transport, streaming upload and installation; test partial writes and
   reconnects before physical-device acceptance.
4. Expose account/sign/install in GPUI and CLI; integrate installation state and refresh.
5. Complete acquisition channels, Apple Silicon conversion, device utilities and private
   integrations. Verify current external availability separately from recovered behavior.

Every architecture-ledger row remains in scope. A row requires implementation and its
fixture/native/live evidence before its completion status changes.

All subsequent code changes follow the repository-wide layout contract in
[AGENTS.md](../AGENTS.md) and [STYLE.md](STYLE.md): separate logical stages visibly, group
related encoding operations, and review readability after automatic formatting. This applies
to every language, crate, test, example and script, not only the requirements encoder.

## Assumption register

| ID | Assumption; dependent result | Stress test / falsification probe |
|---|---|---|
| H1 | Recovered artifacts reflect Sideloadly 0.60 behavior; parity claims depend on this. | Compare [reconstruction](../../SIDELOADLY_DECONSTRUCTED.md), [notes](../../notes/AUTH_NOTES.md) and recovered control flow; record contradictions. |
| H2 | Recovered Apple/private endpoints may still accept the protocol; live-workflow claims depend on this. Current state: **unknown**. | Controlled fixtures followed by authorized, dated live requests. |
| H3 | Decoded profile fields represent authentic Apple-issued contents; trust claims depend on this. The parser does not establish it; `verify_trust` does for the Apple chain/name policy, when requested. | Generated tamper/policy fixtures and three local Apple profiles pass; revocation and on-device policy are not checked. |
| H4 | Prefix, wildcard and UDID handling cover target profiles; selection/device-match claims depend on this. | Legacy prefix ≠ Team ID, absent prefix, exact/wildcard boundaries, mixed-case/hyphenated UDIDs and real samples; compare [TN2318](https://developer.apple.com/library/archive/technotes/tn2318/) and [TN3125](https://developer.apple.com/documentation/technotes/tn3125-inside-code-signing-provisioning-profiles). |
| H5 | Inputs and external replacements remain stable during a job; fidelity/path/determinism claims depend on this. | Mutate same-size inputs and symlinks mid-job; snapshot or reject changed content/metadata. |
| H6 | Generated fixtures predict corresponding real-input behavior; signing/format claims depend on this. | Independent real IPA/profile samples, native tamper checks and physical installation. |
| H7 | One process writes settings and event consumers drain jobs; consistency/memory claims depend on this. | Concurrent writers, stalled-consumer long job, clock changes and crash/restart tests. |

Additional component assumptions and probes are in [APPLE.md](APPLE.md),
[BUNDLE.md](BUNDLE.md), [ENGINE.md](ENGINE.md) and [UI.md](UI.md).

## Bounded observations

- **High impact:** Decoded-field preflight alone cannot establish authenticity; signing verifies
  CMS trust only when requested. Apple ID engine integration and device transport block
  stock-device installation evidence.
- **High impact:** Mock portal/private-service success cannot establish current external
  availability; that status remains unknown until a dated live check.
- **Medium impact:** Child-profile selection, legacy prefixes and target-device matching can
  fail independently of a valid main-app signature.
- **Medium impact:** Unbounded events, whole-binary signing and source TOCTOU affect memory or
  consistency beyond the small-fixture evidence.
- **Low impact:** Document test counts and grouped code layout can drift after edits; derive
  counts from current test output and inspect source after formatting.

## Handover quality gates

QG1: This document reports implementation facts and verification status without a normative
judgment. QG2: H1–H7 include dependent results and probes. QG3: Known architecture-ledger
workstreams are accounted for; unknown service behavior remains marked unknown. QG4: No new
numerical derivation is asserted here; units/limits remain in component documents. QG5:
Uncommitted work, stale documentation and unresolved trust/device cases are explicit. QG6:
Local source/tests, recovered artifacts and linked Apple documents define provenance. QG7:
High/medium/low observations are recorded. These are handover checks; the **project**
completion gates remain open.
