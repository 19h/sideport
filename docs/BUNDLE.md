# Bundle implementation and verification

sl-bundle now prepares IPA, zipped-app and bare-app inputs in an owned temporary tree.
It accepts XOR-0xAA flipped archives, validates the central directory before the ZIP index
is allocated, and decompresses entries in parallel. Reader clones share a file handle and
immutable ZIP metadata, with independent positional offsets.

Preparation APIs require an exclusive mutable borrow. Packing takes a shared borrow, so a
caller cannot concurrently mutate a prepared archive through these APIs while streaming it.

The implementation supports recursive Info.plist properties and transforms, original
identifier records, extension suffixes, URL-name rewrites, localized strings, WatchKit/plugin
removal, SINF path cleanup, file/directory replacement, local dylib/framework/resource
injection, dependency IDs and framework rpaths. Deep signing processes child bundles before
their parents, merges alternate entitlements, signs loose ARM64 code, seals resources, and
supports development identities, ad-hoc signing and stripping.

Inspection reads bounded metadata and executable headers without extraction, including
flipped archives. Declared and legacy PNG icons are selected by decoded size and normalized;
8-bit RGB/RGBA CgBI conversion has generated fixtures. Asset-catalog icons remain pending
and produce an explicit diagnostic. See ENGINE.md for limits and inspection's CRC boundary.

The recovered child-property and localization comparisons are preserved explicitly.
Malformed main bundle code fails the job. Loose-code failures are returned in SignReport
rather than disappearing in a log.

Identity signing requires the main app's provisioning profile. Before any file changes, every
profile that the pass would embed is validated against that bundle's own identifier and the
signing certificate, plus the optional target UDID, platform and trust anchors in
`ProfileRequirements`. A child without its own profile inherits the parent's entitlements and
embeds nothing, as in the recovered `isign.bundle`. Profile failures return
`Error::Profile` naming the bundle. docs/APPLE.md defines the profile rules.

Frameworks inherit entitlements, matching the recovered `isign.signable` and `isign.bundle`
modules. Extension executables retain CS_EXECSEG_MAIN_BINARY: the native fixture
independently confirms that Apple's signer sets it for the same MH_EXECUTE extension.
This differs from the recovered Appex class, which lacks an is_main_binary declaration.

## Output and format sources

Packing emits sorted entries with a fixed DOS timestamp and compression level. The writer
uses data descriptors and emits ZIP64 when required, without seeking or retaining whole
payloads. Format fields follow
[PKWARE APPNOTE 6.3.10](https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT).
Atomic file output keeps the previous destination until the new file is flushed and synced.
Folder output (`save_folder`) writes `<destination>/Payload/<App>.app` with symlinks and modes,
building in a sibling temporary directory that is renamed into place; an existing destination
or one inside the staging tree is refused.

Directory permissions are retained separately while staging directories remain writable.
New-file creation refuses existing filesystem aliases; symlinks are installed after regular
payload writes. Path lengths, depth, metadata/index bytes, entry counts and expanded data
are bounded by configurable ArchiveLimits.

Binary/XML plists and UTF-8/UTF-16/UTF-32 text are accepted. The text parser handles
dictionaries, arrays, data and string escapes, including localized .strings syntax.
The legacy container syntax is described by
[Apple's Property List Programming Guide](https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/PropertyLists/OldStylePlists/OldStylePLists.html).

## Verification

The current three-crate suite has 76 tests: 27 bundle tests, 39 code-signing tests and
10 Mach-O tests, plus one ignored real-profile probe. The five-crate suite including engine/CLI
has 100 tests. Tests generate their own apps, archives, profiles and certificates.

- Rust zip, Python zipfile and Info-ZIP accept identical forward-only output, including forced ZIP64.
- CRC damage, duplicate entries, traversal, filesystem-name collisions, symlink ancestors,
  expansion/index/path limits and understated metadata are rejected.
- Cancellation leaves source bytes and an existing destination unchanged, and interrupts
  valid DEFLATE streams that emit no payload bytes.
- Read-only directory permissions survive editing and repacking.
- Metadata, removal, SINF, injection, idempotent load commands and all four thin formats have fixtures.
- Native codesign verifies nested universal apps/frameworks/extensions strictly after signing
  and after archive round trips. Injected dylib code loads during native execution.
- Nested resource tampering is rejected; stripped bundles can be signed and verified again.
- Identity-signed frameworks/extensions carry inherited XML/DER entitlements and merged overrides.
  That fixture is self-signed and establishes encoding behavior, not Apple trust.
- A mismatched child profile, absent or malformed device, wrong platform, untrusted profile or
  expired profile fails before any bundle file changes; matching child profiles are embedded
  per bundle and frameworks receive none.

Commands:
`cargo test -p sl-bundle -p sl-macho -p sl-codesign`
and `cargo clippy -p sl-bundle -p sl-macho -p sl-codesign --all-targets -- -D warnings`.

## Complexity and assumptions

Let B be compressed plus expanded payload bytes, N entries, L total names, F the sum of registered prefix lengths,
P extraction workers, and W codec workspace. With fixed compression settings:

| Operation | Time | Additional memory |
|---|---|---|
| Archive validation/indexing | O(F log N) | O(F + N + L), subject to index limits |
| Payload extraction | O(B) | O(P × (128 KiB + W)) plus the shared index |
| Sorted forward packing | O(B + N log N) | O(N + L + 128 KiB + W), excluding the caller's sink |
| Metadata traversal | O(N + visited plist bytes) | Traversal depth plus bounded plist data |

These are algorithmic bounds, not throughput measurements. A release benchmark with large
archives and physical-device upload remains required.

- A1: Inputs and external replacement/injection sources remain stable while read.
  Dependent results: deterministic output and copying. Probes: CRC/size checks and cancellation
  fixtures; same-size external changes still require source snapshotting or explicit change detection.
- A2: Native fixture verification establishes the covered signing encodings.
  Dependent results: interoperability claims. Probes: independent decoded slots, Apple codesign,
  native execution and tamper rejection. Physical-device installation remains unverified.
- A3: Host filename handling preserves the tested names. Dependent results: extraction/repacking
  name fidelity. Probes: Unicode normalization/case collisions and exclusive node creation.
  Portable shortening/indirection and Windows-specific long-name/symlink cases remain pending.

## Bounded remaining work

High impact: Apple authentication/provisioning, real device installation, the remaining engine/
CLI workflows and GPUI remain incomplete. Real inspection and local exports are now wired
through engine/CLI and independently verified. ARCHITECTURE.md retains the full ledger.

Medium impact: remote/special/deb injection preparation, icon/asset-catalog editing and
portable filename indirection remain pending. Forward ZIP streaming still
needs cancellation/backpressure/resume integration with AFC.

Low impact: output file replacement uses one atomic temporary-file commit rather than the
original intermediate .bak rotation; failure tests verify that old output survives.
