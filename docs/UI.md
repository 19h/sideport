# Desktop implementation and evidence

The desktop uses Zed's official `gpui` 0.2.2 and the compatible `gpui-component` 0.5.1.
It operates on real `sl-engine` jobs. It exposes original, unsigned, and ad-hoc IPA export;
Apple account and device workflows remain in the architecture ledger.

## Implemented flow

Opening or dropping one IPA, flipped IPA, app ZIP, or app directory starts a cancellable
inspection job. Metadata and decoded icons become the editor's input. A failed inspection
clears the previous source. The editor validates identifiers, typed JSON Info.plist overrides,
extension selections, and app-relative file paths before creating an export job.

Metadata, Watch/extension removal, file sharing, device restrictions, local injections, and
file replacements/deletions map to the engine's existing options. Original mode disables edits
and uses the engine's unchanged-input contract. Inspection and signing run outside the GPUI
executor. Stage events drive progress; preparation/packaging/upload use byte units, signing
and patching use item counts. A total of zero is indeterminate.

The window owns its inspection/export task and cancellation token. Closing an active worker
requests cancellation, waits for the engine result, then removes the window. Quit uses that
same path. Job prompts cover passwords, exact ASCII verification codes, team choices,
confirmation, save paths, and device retries. Password fields are masked. View debug output
excludes input values. Dialogs accept Enter/Escape and occlude controls underneath them.

macOS file dialogs use `rfd`'s owned AppKit panels attached to the GPUI window. Other platforms
and GPUI tests use GPUI's platform prompts. Returning from a picker restores editor focus.
Light, dark, and system appearance preferences persist through the engine settings file;
system appearance changes are observed. Settings persistence remains synchronous and bounded
to 64 KiB; its filesystem latency has not been benchmarked.

Activity retains at most 400 messages, truncated to 4096 Unicode scalar values each. App icons
are shared rather than rebuilt on every render. Export success can reveal the completed file.
The CLI and desktop have distinct executable names to avoid case-insensitive path collisions.

## Verification

GPUI tests drive the real engine and the rendered control hitbox. They cover inspection,
mode selection, edited ad-hoc export, source-byte preservation, failed-source replacement,
cancel-and-join on close while a real export waits for a save prompt, verification-code validation,
secret-free view debug output, and file-edit validation/submission/cancellation through keys.
Draft tests cover unchanged fields, typed override mapping, invalid identifiers, signed-integer
bounds, traversal paths, absent extensions, and original-mode suppression of edits.

The native macOS window and attached open/save panels were inspected through computer use with
a generated universal Mach-O app. Native pointer editing and the export shortcut produced a
3551 B IPA with the requested display name. Python's independent ZIP reader accepted its CRCs
and recorded executable mode 0755. Apple `codesign --verify --strict --deep --all-architectures`
accepted both slices; the executable ran after restoring archive permissions during Python
extraction, and Info.plist tampering was rejected. The source retained its original display name.
Native inspection also covered original-mode disabled fields and light/system appearance;
preference persistence was checked in the isolated fixture directory.

Workspace tests, Clippy with warnings denied, formatting, and native packaging pass for this
implemented subset. Linux and Windows runtime/rendered behavior and physical-device
installability remain unverified.

## Complexity and units

For `j` JSON bytes, `e` available extensions, `s` selected extensions, `p` metadata/path
bytes, and maximum extension-name length `L`, a conservative validation bound is
O(j log(j + 1) + (e + s) L log(e + 1) + p) time. This includes map/set insertion and
string comparisons. Temporary space is O(j + e + p), including cloned options; JSON
above 64 KiB is rejected. Displayed binary sizes use 1 KiB = 1024 B,
1 MiB = 1024² B, and 1 GiB = 1024³ B. Progress percentages are `100 × done / total`
for nonzero totals. Engine inspection/export complexity is documented in ENGINE.md.

The view's activity bound is O(400 × 4096) scalar values; UTF-8 payload length is at most
6,553,600 B. Allocation capacity and container overhead are additional. The engine producer
queue remains unbounded. App metadata,
icon bytes, editing options, and component state are additional allocations.

## Assumption register

- A1: Sources remain stable during inspection and export. Dependent results: source fidelity and
  displayed metadata. Probes: source-byte comparison, archive CRC/extent tests, mutation/snapshot
  checks retained in the engine ledger. The view does not create an immutable source snapshot.
- A2: Engine cancellation reaches a checkpoint before commit. Dependent results: destination
  preservation on cancellation. Probes: real pending-prompt close, engine/CLI cancellation during
  packing, and existing-output comparisons. A completed commit is not rolled back by a late click.
- A3: The compatible control/runtime versions provide the tested focus and layout behavior.
  Dependent results: desktop interaction. Probes: rendered hitbox clicks, Enter/Escape tests,
  native open/save flows, and appearance/resize inspection. Arbitrary OS versions are unverified.
- A4: Generated Mach-O fixtures cover the exercised signing cases. Dependent results: fixture
  export/signature verification. Probe: Apple codesign, execution, tamper rejection, and independent
  archive readers. Apple provisioning trust and stock-device acceptance remain unverified.
- A5: Events are consumed while a job runs. Dependent results: practical producer memory.
  Probe: event throttling and bounded view activity; stalled-consumer queue retention is pending.

## Bounded observations

- High impact: account provisioning, physical devices, refresh, Store/private services, and other
  full-scope workflows still require implementation and evidence in ARCHITECTURE.md.
- Medium impact: GPUI 0.2.2's native accessibility tree exposes the window/menu rather than these
  custom form controls. Native verification therefore also uses screenshots and rendered control
  tests. Full accessibility integration, non-macOS runtime checks, settings-write latency, and
  strict event backpressure remain pending.
- Medium impact: minimum-size scrolling and modal focus matter independently of successful
  exports. Keep rendered/key interaction checks when controls or layout change.
- Low impact: executable names must differ beyond case on macOS. `SideportDesktop` and `sideport`
  are verified together by workspace CLI subprocess tests.

## Primary sources and gates

- [Zed GPUI source](https://github.com/zed-industries/zed/tree/main/crates/gpui) and the pinned
  [0.2.2 API](https://docs.rs/gpui/0.2.2/gpui/).
- [GPUI Component 0.5.1](https://docs.rs/gpui-component/0.5.1/gpui_component/) and its packaged
  Cargo manifest declaring GPUI 0.2.2 compatibility.
- [RFD source](https://github.com/PolyMeilex/rfd), pinned 0.17.2 AppKit panel ownership and async API.
- [Apple AppKit NSOpenPanel](https://developer.apple.com/documentation/appkit/nsopenpanel).

QG1–QG7 apply to the implemented subset with the assumptions and verification boundaries above.
The full objective's coverage gate remains open wherever the architecture ledger lists missing work.
