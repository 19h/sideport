# Sideport architecture and implementation evidence

The objective is a complete Rust implementation of the behavior documented in
`../../SIDELOADLY_DECONSTRUCTED.md` and `../../notes/`, with a GPUI desktop interface.
The reconstruction describes Sideloadly 0.60; it is evidence for client behavior, not proof
that every recovered endpoint or operating-system integration still works.

No documented workflow is excluded from the objective. Private services require configurable
endpoints and credentials, protocol fixtures, and an explicit live-verification status. Their
server implementations are unknown.

## Requirements and evidence

| Requirement | Source | Current evidence | Evidence still required |
|---|---|---|---|
| Thin/fat Mach-O parsing, commands, dual CodeDirectories, requirements, XML/DER entitlements, CMS, resource seals | CODESIGN_NOTES; report §10 | Implemented in sl-macho/sl-codesign; 30 tests, Apple codesign and OpenSSL interoperability; forward header inspection | Real development identity/profile/device acceptance; broader malformed-format coverage |
| IPA, zipped-app and directory inputs; flipped IPA; filename portability; pruning; recursive metadata edits; replacements; icons | BUNDLE_NOTES §§2–5; report §7 | Implemented extraction, header/metadata inspection, flipped input, pruning, recursive metadata and replacements; PNG/CgBI inspection fixtures | Portable filename indirection, icon editing/asset handling; arbitrary Apple CgBI interoperability; additional platform verification |
| Library/framework/resource injection; dependency/rpath rewrites; remote/special sources; ar and compressed deb packages | BUNDLE_NOTES §6; report §7.4 | Local library/framework/resource copying and dependency/rpath rewriting implemented; native injected code executes | Remote/special source resolution and deb/ar/compression preparation |
| Child-before-parent signing, provisioning, entitlement merging, unsigned/original/ad-hoc modes | BUNDLE_NOTES §7; report §§3,7.5–7.6 | Deep traversal, profiles and merged/inherited entitlements implemented; nested universal codesign verification; real engine/CLI original/unsigned/ad-hoc exports | Real Apple profiles/device acceptance; identity/provisioning engine integration |
| Deterministic IPA, forward streaming, atomic file and folder outputs | BUNDLE_NOTES §8; report §7.7 | Forward-only deterministic ZIP/ZIP64 and atomic files verified by three readers; cancellation preserves existing output | Folder exports and resumable device upload integration |
| Local AOSKit, Mail plugin notifications/kbsync, remote anisette and fallback policy | AUTH_NOTES; report §4 | Real bounded remote provider, shared cache, engine/CLI checks; HTTP, cancellation, clock, decompression and validation tests; docs/APPLE.md | Native AOSKit/Mail bridge, fallback selection, private provider integration and live checks |
| GSA SRP, legacy IDMS, app tokens, trusted-device/SMS 2FA, session migration/persistence | AUTH_NOTES; report §5 | SRP/negotiation/CBC/GCM primitives and bounded GSA init/complete/apptokens transport; eight independent Python vectors; mock-server alternate-anisette, second-factor and cancellation tests; non-demo engine login/prompt bridge and memory-only session; docs/APPLE.md | Legacy IDMS, account UI/CLI, session migration/persistence and live account verification; uncertain second-factor branches require live parity checks |
| Portal teams, devices, certificates/CSR/reuse/revocation, app IDs, profiles, free/paid/tvOS policies | AUTH_NOTES; report §§3,6 | Typed QH65B2 client for core list/create/revoke/download actions; bounded XML transport and six portal tests; engine attempts team enumeration; CSR/key/identity/profile primitives | End-to-end provisioning policy and engine/UI workflows, remaining actions, session expiry/retry, and live account/device verification |
| USB/Wi-Fi discovery, lockdown/pairing, AFC resumable file/ZIP upload, installation retry/progress | MOBDEV_NOTES; report §8.1 | Device crate is a stub; idevice dependency present | Real implementation, simulated failures and physical-device verification |
| Apps/profiles management, syslog, Developer Disk Images and JIT | MOBDEV_NOTES; report §12 | Pending | Protocol tests and physical-device verification |
| Apple Silicon conversion, entitlement adjustments, SINF enrichment and application installation | report §8.3; reconstructed Go | Pending | Implementation and native Mac verification |
| URI/download channels, HTTP resume, App Store authentication/purchase/download, FairPlay metadata/kbsync | report §9; reconstructed Go | Pending | Client implementation, controlled transport fixtures and live service verification |
| Stored files, installations DB, refresh policy/scheduler, tray/autostart and local IPC | report §§11,14 | Real export/inspection engine; bounded atomic settings persistence; runtime/job/prompt cancellation and restart fixtures | Accounts/sessions/installations DB, refresh, expiry, tray/autostart and IPC integration |
| Feature tokens, Patreon OAuth, private remote providers and update/version protocol | report §§3,4.3,13 | Pending | Configurable client implementations, recovered protocol fixtures; server behavior remains unknown |
| CLI and intuitive GPUI interface covering the workflows above | report §§2–3,11–12; objective | CLI inspection/export/JSON/typed edits/injection/cancellation; official Zed GPUI inspection/editor/export with native pickers, progress, prompts, cancellation and themes; real engine/GPUI interaction tests and native window inspection | Remaining CLI/desktop workflows; complete accessibility, additional platform/runtime and full account/device user-flow evidence |

This ledger records verification boundaries. A passing crate test is evidence for that test's
covered behavior, not completion of a workflow that depends on accounts, devices or native services.

## Crate responsibilities

- sl-macho: checked binary parsing and load-command rewriting.
- sl-codesign: identities, profiles, signature encodings and resource seals.
- sl-bundle: archive preparation, bundle editing, injection and ordered signing.
- sl-apple: authentication, anisette, portal and Store clients.
- sl-device: device transports, services, installation and utilities.
- sl-engine: jobs, policies, storage, refresh, IPC and integration.
- sl-cli: scriptable commands.
- sl-app: GPUI desktop interface.

Network/device work runs on the engine's Tokio runtime. CPU-intensive work runs outside the
UI executor. Front ends consume typed job events and prompts. Operations need cancellation
checks inside transfer, extraction, hashing and packing loops. Storage and exported output
must remain recoverable after failures.

## Assumption register

- A1: Recovered constants and control flow describe Sideloadly 0.60 accurately enough to guide
  implementation. Dependent results: parity claims. Probe: compare notes, reconstructed Python,
  decompiled Go and generated fixtures; resolve contradictions explicitly.
- A2: A recovered external protocol may still be served. Dependent results: live authentication,
  provisioning, download and private-service claims. Probe: controlled integration followed by
  authorized live verification. Until checked, current availability is unknown.
- A3: Native codesign/OpenSSL acceptance establishes encoding interoperability for the fixtures.
  Dependent results: signing-format validation. Probe: independent byte decoding, tamper rejection
  and physical-device installation. Native fixture acceptance does not establish Apple trust.
- A4: Input files remain stable while processed. Dependent results: parallel extraction and repeatable
  output. Probe: capture and compare source metadata, reject inconsistent entry sizes and CRCs,
  and test interrupted/mutated input.

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
