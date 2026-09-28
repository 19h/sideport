# Engine and CLI evidence

`Engine::inspect` dispatches bounded archive metadata, executable-header, and icon reads to
a blocking worker. IPA/app ZIP inspection uses the same central-directory preflight and
path validation as preparation. It does not extract the app or read every payload.
BinaryMetadata covers thin/fat 32/64-bit containers and both byte orders; it reads load
commands and reports encryption from all architectures. This is header inspection, not
complete Mach-O or ZIP validation.

PNG selection reads declared primary iPhone/iPad icon filenames and their scale/device
suffixes, with a legacy Icon*.png fallback. The largest successfully decoded image is
returned as ordinary RGBA PNG. CgBI conversion handles raw DEFLATE, BGRA channel order,
and premultiplied alpha for 8-bit RGB/RGBA. Invalid optional icons produce diagnostics;
asset-catalog-only icons produce an explicit pending diagnostic.

Metadata is limited to 16 MiB per plist, icon input to 32 MiB, icon dimensions to
4096 × 4096 pixels, and CgBI scanlines to 80 MiB. The image decoder has a 128 MiB
allocation limit; that is not a total-process peak-memory bound. Mach-O inspection limits
the architecture table to 128 records and each slice's load commands to 16 MiB.

## Job behavior

The real export pipeline supports original, unsigned, and ad-hoc modes. It maps public
AppOptions to recursive plist edits, WatchKit/plugin/selected-extension removal, replacements,
and local library/framework/resource injection. "All extensions" removes both PlugIns and
Extensions. Default preparation removes WatchKit containers. The serde defaults retain that
policy when loading older option records.

Original file inputs are copied byte-for-byte through an atomic temporary file. Original
directory inputs are copied and repacked with no metadata edits or signature changes.
Unsigned preparation strips signatures; ad-hoc preparation signs children before parents.
Output cannot overwrite the source or be placed inside a source app directory.

Workers check cancellation cooperatively. The job future awaits the worker's termination
before returning a cancelled result. Dropping a job handle cancels its token. Pending prompts
finish on cancellation even if the front end retains the prompt without answering it.
The runtime remains owned by running operations after an Engine handle is dropped; shutdown
does not panic inside an async caller. Subscriber senders are owned and released, rather than leaked.

Progress uses the maximum received count within a stage/total and emits changed intermediate
counts at most once per 100 ms, plus the initial and terminal counts. This prevents a parallel
callback per chunk from filling the event queue. The event channel remains unbounded: a stalled
consumer can still accumulate logs and rate-limited progress over a long job.

Settings load/save uses bounded JSON and an atomic same-directory temporary file. Memory is
updated only after the disk write commits. Invalid settings are reported instead of overwritten.
Accounts, teams, certificates, installations, the refresh queue and stored-file records live in
`state.sqlite3` (schema version 1; a newer schema is refused). Secrets live in the keychain or a
0600 file (docs/APPLE.md). Installations can be listed, toggled, refreshed and forgotten;
forgetting deletes an unreferenced stored IPA copy. Device jobs, refresh and the scheduler are
described in docs/DEVICE.md.

Apple ID export (`SigningMode::AppleId` with `Target::ExportIpa`) inspects the input, runs the
provisioning policy in docs/APPLE.md, asks for the output path, then patches, injects and signs
with the issued identity and downloaded profiles on a blocking worker. The outcome carries the
profile expiry. User entitlement overrides are merged over each profile's entitlements, as the
recovered alternate-entitlements option does; keys whose values the profile does not grant (by
equality or a trailing-asterisk grant) produce warnings rather than silent drops.

## Algorithm and complexity

The export sequence is: validate the requested backend/options; inspect on a blocking worker;
resolve the output path; create private staging; apply removals/replacements/metadata edits;
prepare injections; sign or strip; pack into a temporary output; sync and atomically commit;
report the committed outcome. Original archive mode replaces the staging/edit/sign/pack steps
with a bounded-buffer byte copy. Cancellation checks precede output commit.

Let N be entries, F the archive index's registered path-prefix bytes, S compressed plus expanded
bytes consumed by inspection, and Q decoded icon pixels. Inspection costs O(F log N + S + Q)
time; its additional storage is the bounded index, plist/icon/header buffers, and codec workspace.
For a universal binary in a ZIP, S includes decompressed gaps before subsequent slice headers.
Original file copying is O(B) time with a 128 KiB transfer buffer for B source bytes; inspection
costs are additional. Preparation/packing bounds are in BUNDLE.md. Signing currently retains
whole binary buffers, so concurrent framework signing can multiply the largest-binary memory cost.
These are algorithmic bounds; measured throughput, peak RSS, and cancellation latency remain unknown.

## CLI and primary sources

`sideport` exposes the engine: `inspect`, `export` (ad-hoc, unsigned, original, Apple ID),
`install`, `run`, `settings`, `account` (list, login, logout, import), `certificates`,
`app-ids`, `registered-devices`, `devices`, `device` (apps, uninstall, profiles,
remove-profile, pair), `installations`, `installation` (refresh, forget, auto-refresh),
`refresh-due` and `daemon`. Terminal prompts cover every prompt kind; without a terminal they
are declined. Hidden options select fixture origins, profile anchors and file secrets for
subprocess tests. `inspect`, `export`, and `run` use the real engine. Export exposes typed plist edits,
identifier/name/version/OS changes, extension policy, file sharing, device restriction removal,
local injection, replacement/deletion, progress, cancellation, and save-path prompts. JSON
results go to stdout; job diagnostics go to stderr. Inspection JSON omits icon pixel bytes.
Unattended exports require an output path. Custom icons, unsigned device installs, and
entitlement overrides without Apple ID signing return explicit unsupported errors before output
mutation.

Bundle identifiers and primary icon declarations follow
[Apple's Core Foundation Keys reference](https://developer.apple.com/library/archive/documentation/General/Reference/InfoPlistKeyReference/Articles/CoreFoundationKeys.html).
Runtime lifetime/shutdown follows
[Tokio Runtime](https://docs.rs/tokio/1.53.1/tokio/runtime/struct.Runtime.html#method.shutdown_background).
SIGINT handling uses
[ctrlc set_handler](https://docs.rs/ctrlc/3.5.2/ctrlc/fn.set_handler.html).
Archive and plist sources are recorded in BUNDLE.md. CgBI lacks an identified normative
Apple format specification here; conversion evidence is the generated byte fixture and ordinary-PNG
decoder output. Compatibility with arbitrary Apple-optimized PNGs remains unproven.

The desktop uses Zed's official [GPUI 0.2.2 crate](https://docs.rs/gpui/0.2.2/gpui/).
UI.md records its implementation, primary sources, and rendered evidence.

## Verification and assumptions

The five-crate suite currently has 125 tests: sl-bundle 27, sl-codesign 39, sl-macho 10,
sl-engine 42, and sl-cli 7. The full workspace also runs sl-apple 33, sl-device 13 and sl-app 8,
for 179, plus three ignored local probes. The engine authentication/portal and CLI anisette tests are
described in APPLE.md. Native generated universal code is exported through the real engine,
accepted by Apple's codesign with strict/deep/all-architecture verification, and executes.
Info.plist tampering is rejected. CLI subprocess tests cover metadata/export JSON and SIGINT
cancellation during packing while preserving an existing output. Engine fixtures cover real
option mapping, source preservation, save prompts, runtime lifetime, settings restart/failure,
prompt cancellation, subscriber closure, and concurrent progress reduction.

- A1: Input files, symlinks, and external injection/replacement sources remain stable while read.
  Dependent results: source fidelity, header/metadata consistency, deterministic output, path checks.
  Probes: CRC/size checks, source-byte comparisons, symlink escape tests, nonregular-file rejection;
  same-size mutations and TOCTOU changes still need snapshot/detection tests.
- A2: Generated fixtures represent the implemented format cases. Dependent results: inspection,
  encoding, CgBI, and native signing claims. Probes: both widths/orders, fat variants, malformed
  extents/CRC/dimensions, native codesign/execution/tamper checks. Real device/profile trust is unverified.
- A3: Cancellation reaches a checkpoint before output commit. Dependent results: cancelled jobs
  preserve an existing destination. Probes: engine and CLI cancellation during packing and prompt
  cancellation. Once the atomic commit succeeds, the job returns success; an interrupt racing after
  that commit does not roll the output back. Resource sealing and single signing calls still need
  finer-grained cancellation checkpoints and latency measurements.
- A6: SQLite WAL locking serializes processes sharing a data directory. Dependent results:
  one machine UUID and signing key per data directory, consistent installation rows. Probe:
  two-connection metadata agreement and exclusive key creation; multi-process crash tests remain.
- A4: One Engine process writes a given settings file. Dependent results: serialized settings updates.
  Probes: restart, invalid JSON, oversized writes, memory/disk preservation. Cross-process locking and
  settings conflict resolution remain pending.
- A5: The event consumer drains the job while it runs. Dependent results: practical event-buffer memory.
  Probe: concurrent callback reduction is tested; prolonged stalled-consumer retention remains to be
  replaced or bounded explicitly.

## Bounded observations

High impact: Apple authentication/provisioning, physical-device acceptance, remaining desktop workflows, background refresh,
Store/private-service clients, and the remaining full-scope workflows are not complete. The architecture
ledger retains those requirements. A native ad-hoc fixture does not establish device installability.

Medium impact: asset-catalog icons, icon editing, filename portability, local identity loading in the
engine, backpressure, cancellation latency, and cross-process settings writes need further work.
Header-only inspection deliberately leaves unvisited payload CRCs unchecked until preparation.
It also allows large inputs to be reviewed before paying the full extraction/signing cost.

Low impact: original archive output preserves source bytes and metadata; prepared output uses the
deterministic writer's timestamps and permissions. These are different export contracts, covered by fixtures.

The current change's coverage is verified by the commands in README.md. The full objective's
quality/completion gates remain open wherever the architecture ledger requires missing evidence.
