# Sideport architecture and implementation evidence

The objective is a complete Rust implementation of Sideloadly 0.60's client behavior, as
recovered by reverse engineering, with a GPUI desktop interface. The recovered behavior is
evidence for how that client worked, not proof that every recovered endpoint or operating-system
integration still works. The reverse-engineering material is not part of this repository; the
contracts implemented so far are captured in these documents, the source and its tests.

No documented workflow is excluded from the objective. Private services require configurable
endpoints and credentials, protocol fixtures, and an explicit live-verification status. Their
server implementations are unknown.

## Requirements and evidence

| Requirement | Current evidence | Evidence still required |
|---|---|---|
| Thin/fat Mach-O parsing, commands, dual CodeDirectories, requirements, XML/DER entitlements, CMS, resource seals, profile validation/trust | Implemented in sl-macho/sl-codesign; 49 tests, Apple codesign and OpenSSL interoperability; forward header inspection; profile field validation and CMS chain/policy verification with generated and local real-profile probes (docs/APPLE.md) | Real development identity/device acceptance; broader malformed-format coverage |
| IPA, zipped-app and directory inputs; flipped IPA; filename portability; pruning; recursive metadata edits; replacements; icons | Implemented extraction, header/metadata inspection, flipped input, pruning, recursive metadata and replacements; PNG/CgBI inspection fixtures; custom loose-PNG icon replacement resized to each declared size (Info.plist untouched, Assets.car refused as recovered); recovered `%%<n>`/`.filenames_mangled` indirection implemented as a tested utility | Wiring the mangling utility into unpack/pack (not required for Sideport); `Assets.car` icon editing (intentionally not done); arbitrary Apple CgBI interoperability; cross-platform round-trips |
| Library/framework/resource injection; dependency/rpath rewrites; remote/special sources; ar and compressed deb packages | Local library/framework/resource copying and dependency/rpath rewriting implemented; native injected code executes; `.deb`/`ar` inputs decoded (gz/xz/lzma/bz2/zst) with the recovered selection and escape rejection; `http(s)://` and `///special/{substrate,substitute,spoofer}` sources resolved through configurable endpoints and unpacked; engine/CLI wired with wiremock fixtures | Real tweak packages, live special hosts, and native execution of injected/rebuilt binaries on a device |
| Child-before-parent signing, provisioning, entitlement merging, unsigned/original/ad-hoc modes | Deep traversal, profiles and merged/inherited entitlements implemented; every embedded profile preflighted against its own bundle ID, optional device/platform/trust before mutation; nested universal codesign verification; real engine/CLI original/unsigned/ad-hoc exports | Real Apple profiles/device acceptance; identity/provisioning engine integration |
| Deterministic IPA, forward streaming, atomic file and folder outputs | Forward-only deterministic ZIP/ZIP64 and atomic files verified by three readers; cancellation preserves existing output; folder output; resumable device streaming (docs/DEVICE.md) | Physical-device streaming verification |
| Local AOSKit, Mail plugin notifications/kbsync, remote anisette and fallback policy | Remote provider with cache and tests; `sl-macos` AOSKit bridge and local header assembly; per-job local→alternate fallback; engine/CLI checks; AOSKit refused requests on this Mac (docs/APPLE.md) | Mail plug-in bridge, kbsync, private provider integration and live checks |
| GSA SRP, legacy IDMS, app tokens, trusted-device/SMS 2FA, session migration/persistence | SRP/negotiation/CBC/GCM primitives and bounded GSA transport; eight independent Python vectors; mock-server alternate-anisette, second-factor and cancellation tests; engine login with keychain/file-persisted sessions and remembered passwords, restart restore, recovered `sessions.json` import and 1100 renewal; docs/APPLE.md | Legacy IDMS, account UI/CLI and live account verification; uncertain second-factor branches require live parity checks |
| Portal teams, devices, certificates/CSR/reuse/revocation, app IDs, profiles, free/paid/tvOS policies | Typed QH65B2 client; engine provisioning with device registration, certificate reuse by public key, CSR, confirmed 7460 revocation, App ID reuse/creation and free quota, trust-verified profile download, recovered bundle-ID policy, tvOS and per-extension options; Apple ID IPA export; stateful fake-portal and native codesign/OpenSSL checks | Device-target provisioning, UI/CLI workflows, and live account/device verification |
| USB/Wi-Fi discovery, lockdown/pairing, AFC resumable file/ZIP upload, installation retry/progress | `sl-device` usbmuxd discovery/watch, lockdown values, pairing, AFC staging, framed installation proxy, recovered retry policy, deterministic ZIP streaming with backpressure; engine device jobs; 11 fault-injection and 4 engine fixtures; read-only probe of a USB iPhone (docs/DEVICE.md) | Physical installation, Wi-Fi and tvOS PIN pairing verification |
| Apps/profiles management, syslog, Developer Disk Images and JIT | App list/uninstall and profile list/remove through the device layer with fixtures | Syslog, Developer Disk Images, JIT and physical verification |
| Apple Silicon conversion, entitlement adjustments, SINF enrichment and application installation | Mac listed as a device by provisioning UDID; Apple ID provisioning with registration and mangling; folder output; recovered `Wrapper`/`WrappedBundle`/tag conversion and placement with tag, bundle-ID and name rules; SINF enrichment in `sl-acquire`; fixture test (docs/DEVICE.md) | Launch verification with an Apple-issued identity; Mac-specific entitlement adjustments |
| URI/download channels, HTTP resume, App Store authentication/purchase/download, FairPlay metadata/kbsync | `sl-acquire`: `sideloadly:` link parser with the recovered rules/messages and 155-country table, resumable Range downloads with recovered backoff, HTML/non-ZIP rejection, MD5/SHA-1 verification, flipped storage and `EnrichIpa`; engine/CLI accept links as job sources; 13 fixtures and an engine test (docs/ACQUIRE.md) | App Store authentication/purchase/download and kbsync; live link sources |
| Stored files, installations DB, refresh policy/scheduler, tray/autostart and local IPC | SQLite accounts/certificates/installations with content-addressed stored inputs; device jobs record installations; refresh replay; scheduler with due selection, reachability and cross-process claims; fixtures | Tray/autostart, local IPC, clock-shift and crash tests |
| Feature tokens, Patreon OAuth, private remote providers and update/version protocol | Implemented in `sl-services`: BSDIFF40 `bspatch` (cross-checked against `/usr/bin/bspatch`), a configurable `go-selfupdate` manifest/patch/full-binary update client with SHA-256 verification and the recovered `.new`/`.old`/`.bak` swap, and a generic RS256 feature-token verifier with a Sideport-defined claim schema; engine `ServiceConfig`, `services_status`/`check_update`/`apply_feature_token` and CLI `services status`/`check-update`; 20 crate tests plus a CLI test (docs/SERVICES.md). No endpoints or keys are configured by default and no private service is contacted; the private remote anisette provider is the existing `RemoteAnisette` | Operator-supplied endpoints/key and dated live verification; feature-gating of provider/option selection; the recovered Patreon-anisette endpoint, feature-token key and claim names are deliberately not extracted; server behavior/availability unknown |
| CLI and intuitive GPUI interface covering the workflows above | CLI inspection/export/JSON/typed edits/injection/cancellation; official Zed GPUI inspection/editor/export with native pickers, progress, prompts, cancellation and themes; real engine/GPUI interaction tests and native window inspection | Remaining CLI/desktop workflows; complete accessibility, additional platform/runtime and full account/device user-flow evidence |

This ledger records verification boundaries. A passing crate test is evidence for that test's
covered behavior, not completion of a workflow that depends on accounts, devices or native services.

## Crate responsibilities

- sl-macho: checked binary parsing and load-command rewriting.
- sl-codesign: identities, profiles, signature encodings and resource seals.
- sl-bundle: archive preparation, bundle editing, injection and ordered signing.
- sl-apple: authentication, anisette, portal and Store clients.
- sl-device: device transports, services, installation and utilities (docs/DEVICE.md).
- sl-acquire: `sideloadly:` links, resumable downloads and IPA enrichment.
- sl-services: recovered private-service client — BSDIFF40 `bspatch`, the update protocol and the feature-token verifier (docs/SERVICES.md). Contacts nothing by default.
- sl-macos: AOSKit local anisette and the Mac's identity (the only crate with `unsafe` Objective-C calls).
- sl-testkit: test-only generated PKI, signed profiles, fake portal and fake device layer.
- sl-engine: jobs, policies, storage, refresh, IPC and integration.
- sl-cli: scriptable commands.
- sl-app: GPUI desktop interface.

Network/device work runs on the engine's Tokio runtime. CPU-intensive work runs outside the
UI executor. Front ends consume typed job events and prompts. Operations need cancellation
checks inside transfer, extraction, hashing and packing loops. Storage and exported output
must remain recoverable after failures.

## Assumption register

- A1: Recovered constants and control flow describe Sideloadly 0.60 accurately enough to guide
  implementation. Dependent results: parity claims. Probe: cross-check independently recovered
  constants and control flow against generated fixtures; resolve contradictions explicitly.
- A2: A recovered external protocol may still be served. Dependent results: live authentication,
  provisioning, download and private-service claims. Probe: controlled integration followed by
  authorized live verification. Until checked, current availability is unknown.
- A3: Native codesign/OpenSSL acceptance establishes encoding interoperability for the fixtures.
  Dependent results: signing-format validation. Probe: independent byte decoding, tamper rejection
  and physical-device installation. Native fixture acceptance does not establish Apple trust.
- A4: Input files remain stable while processed. Dependent results: parallel extraction and repeatable
  output. Probe: capture and compare source metadata, reject inconsistent entry sizes and CRCs,
  and test interrupted/mutated input.
- A5: Profile field checks predict device matching, and Apple-root CMS verification establishes
  profile authenticity at signing. Dependent results: identity-signing preflight. Probe: focused
  boundary fixtures, generated tamper/policy chains and local real Apple profiles; device
  installation remains the acceptance test. See docs/APPLE.md A14/A15.

## Bounded observations

- High impact: nested signing and portal policies must be tested together; correct binary signatures
  alone do not establish installable bundles.
- High impact: the reconstruction includes services whose current behavior is unknown. Model and
  test the recovered client protocol without reporting unperformed live checks as complete.
- Medium impact: platform filename normalization and symlink semantics affect extraction and output
  names. Include collision, escape, Unicode and long-name cases in archive verification.
- Medium impact: streaming determinism governs resumable uploads. Verify identical complete bytes
  and suffixes across repeated runs, including ZIP64.
- Low impact: rustfmt can erase encoding groups. Follow AGENTS.md and review the formatted source.

## Quality gates

Before completion, every ledger row requires current implementation and verification evidence.
Record assumptions and probes, resolve contradictions, use primary format/protocol sources, verify
units and calculations where applicable, and review the observations above. UI completion requires
runtime/rendered evidence; a compiling GPUI dependency is insufficient.
