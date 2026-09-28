# Desktop implementation and evidence

The desktop uses Zed's official `gpui` 0.2.2 and the compatible `gpui-component` 0.5.1.
It operates on real `sl-engine` jobs. It exposes original, unsigned, ad-hoc and Apple ID
export; Apple ID, ad-hoc and original installation on a device; Apple ID accounts, devices,
tracked installations and refresh settings. Workflows not listed here remain in the
architecture ledger.

## Implemented flow

The header switches between five sections: App (the editor), Accounts, Devices, Installations
and Settings (⌘1–⌘4 and ⌘, on macOS). Opening or dropping an app always returns to the editor.
`SideportDesktop --demo` runs the engine's simulated accounts, devices and installations.

Opening or dropping one IPA, flipped IPA, app ZIP, or app directory starts a cancellable
inspection job. The empty App section also takes a `sideloadly:` link or HTTP(S) IPA URL
(Download or Enter): the engine's download job runs with the shared progress, then the
downloaded IPA is inspected. The packaged app registers the recovered `sideloadly` URL scheme
and the `com.apple.itunes.ipa` document type; URLs the system opens the app with are queued
until the window exists, `file:` URLs open directly and other URLs download. One app serves
local IPC per data directory (docs/ENGINE.md, Local IPC): a second launch hands its file to the
running window through `/raise` and exits; `/restart` closes the window through the normal
cancellation path. Metadata and decoded icons become the editor's input. A failed inspection
clears the previous source. The editor validates identifiers, typed JSON Info.plist overrides,
extension selections, app-relative file paths and the upload chunk size before creating a job.

The editor's Signing card offers Apple ID, ad-hoc, unsigned and original signing. Apple ID
signing adds an account selector (with guidance and an "Open Accounts" button when no account
exists), the bundle-identifier policy (Automatic, Keep original, or Custom from the identifier
field), an entitlements plist picker and per-extension provisioning. The Destination card
chooses "Export IPA" or "Install on device"; installation lists devices from the engine's device
subscription (name, model, OS, USB/Wi-Fi, unpaired state with a Pair button) and offers stream
upload, upload chunk MiB (1–64), tracking for automatic refresh and tvOS provisioning for
Apple TV. Options that the chosen mode or target does not accept are left at their defaults, so
entitlements never reach a non-Apple ID job and device options never reach an export. The
primary action reads "Export IPA…", "Export original…" or "Install"; it is disabled with a
stated next step while no account, no paired device, or an unsigned install is selected.

Metadata, Watch/extension removal, file sharing, device restrictions, local injections, and
file replacements/deletions map to the engine's existing options. Original mode disables edits
and uses the engine's unchanged-input contract. Inspection and signing run outside the GPUI
executor. Stage events drive progress; preparation/packaging/upload use byte units, signing
and patching use item counts. A total of zero is indeterminate. Job facts (team, final bundle
identifier, free-team App ID quota, profile expiry and TTL, and the anisette machine) are shown
beside the progress. A reported quota is also kept for the job's account.

Accounts lists each account's Apple ID, teams with the default team, session state, remembered
password and last sign-in. Sign-in takes an Apple ID, an optional password and a remember
choice (default from Settings); the password field is cleared when the job starts, and the
job's prompts complete the sign-in. "Import Sideloadly sessions" reads the recovered
`sessions.json` location, or a chosen file, on a background task and lists imported and skipped
entries. Each account can list its development certificates (with a "This Mac" marker for
`is_ours`) and revoke one after a destructive confirmation, list its App IDs, and sign out.

Devices lists attached devices with pairing state; Pair waits for "Trust This Computer?" and
relists. The selected device's user apps (Uninstall) and provisioning profiles (Remove) are
listed; both removals ask first. The device selection is shared with the editor's destination.
Device discovery starts when Devices or the install destination is first shown: an initial
`devices()` listing, then subscription snapshots.

Installations shows each tracked installation's icon, name, version, bundle identifier, device,
Apple ID, team, remaining time (whole days, hours on the final day, "Expired N days ago" in the
danger color), failure count and last error. The automatic-refresh checkbox saves immediately;
"Refresh now" runs the engine's refresh job with the shared progress and prompts; "Forget" asks
first. Refresh notifications from the engine (scheduler or manual) update the list and a notice.

Settings keeps appearance (saved immediately) and adds the anisette provider (this Mac or a
remote URL), an optional alternate remote URL, a provider test, automatic refresh (enabled,
threshold 1–720 h, interval 1–1440 min, Wi-Fi allowed) and the remember-password and
stream-upload defaults. These are validated and applied together by "Save settings" to the
settings as currently stored, so a change another process saved meanwhile (CLI, daemon) is kept;
the saved appearance is preserved. "Keep refreshing after this window closes" installs or removes
the login item at once; it runs the `sideport daemon` tool packaged beside the app, and is
disabled without that tool and in the demo. Choosing this Mac's provider explains that it is tried first and that
the alternate server is used when macOS refuses it (docs/APPLE.md, local anisette).

The window owns one job slot (inspection, export, installation, sign-in, certificate/App ID
listing, revocation or refresh) and its cancellation token. Closing an active job requests
cancellation, waits for the engine result, then removes the window. Quit uses that same path.
Device listing, pairing, uninstalling, profile removal, sign-out and session import are futures
without cancellation; closing the window drops their UI tasks and does not wait for them.

Every `PromptKind` is handled. Passwords are masked with a reveal toggle and return the remember
choice. Verification codes must be exact ASCII digits; "Text me a code" appears only when the
prompt allows SMS and replies `RequestSms`. Teams are chosen from a list (`Choice(index)`).
Confirmations use the engine's label; destructive ones use the danger style, ignore Enter and
require a click. Declining a confirmation replies `Confirmed(false)`; every other dismissal
replies `Cancel`. Save-path prompts accept a typed path or the save panel. Device prompts name the
device (the engine sends its lockdown name) and offer "Retry now". The engine withdraws a
question it stops waiting for (`PromptWithdrawn`, for example when the device returns); that
closes exactly that dialog. Any further job event also closes a device prompt, a new stage closes
any prompt, and a finished job closes its prompt. The app's own destructive confirmations
(revoke, uninstall, remove profile, forget) use the same dialog. View debug output excludes
input values. Dialogs accept Enter/Escape and occlude controls underneath them.

macOS file dialogs use `rfd`'s owned AppKit panels attached to the GPUI window. Other platforms
and GPUI tests use GPUI's platform prompts. Returning from a picker restores editor focus.
Light, dark, and system appearance preferences persist through the engine settings file;
system appearance changes are observed. Settings persistence remains synchronous and bounded
to 64 KiB; its filesystem latency has not been benchmarked. Account, installation and settings
reads and the auto-refresh toggle and forget writes also run synchronously on the UI thread.

Activity retains at most 400 messages, truncated to 4096 Unicode scalar values each. App icons
are shared rather than rebuilt on every render; installation icons are rebuilt only when the list
is reloaded. Export success can reveal the completed file. The CLI and desktop have distinct
executable names to avoid case-insensitive path collisions.

## Verification

`scripts/cargo-ui.sh test -p sl-app` runs 25 tests: 18 GPUI window tests, 2 unit tests and 5
draft integration tests. GPUI tests click the rendered control hitboxes found by debug selectors
and use keystrokes; test engines use file secrets, no scheduler and a fake device layer, so they
never touch the keychain or the system usbmuxd.

- Existing editor tests (5): inspection, mode selection, edited ad-hoc export and source-byte
  preservation, failed-source replacement, cancel-and-join on close while a real export waits for
  a save prompt, verification-code validation with secret-free debug output, and file-edit
  validation/submission/cancellation through keys.
- Prompts (5): SecondFactor with and without SMS (`RequestSms` and a typed code); Password masked,
  Enter validation, remember checkbox and secret-free debug output; ChooseTeam by click;
  destructive Confirm styling, Enter ignored, cancel as `Confirmed(false)`, click as
  `Confirmed(true)`, non-destructive Enter/Escape; WaitForDevice naming the device, "Retry now",
  and closing on progress, on a new stage and on job completion, while a password prompt survives
  progress.
- Demo engine (3): sign-in through the password and verification-code prompts including "Text me
  a code", the account list and badges, certificates and a confirmed revocation, App IDs, sign-out
  and the demo's import refusal; Apple ID device installation through the team prompt with the
  team, bundle identifier, quota and expiry facts rendered and the new installation listed;
  installations with the auto-refresh toggle saved, "Refresh now" completing, forget confirmed by
  click after Enter was ignored, and a refresh-failure notice.
- Real engine (4): pairing an unpaired fake device through the rendered Pair button, then listing
  its apps and a generated signed profile, a confirmed uninstall and a cancelled then confirmed
  profile removal; guidance when no device is attached; guidance without an account and for
  unsigned installs; and settings validation and persistence (invalid threshold rejected without
  writing; provider, alternate URL, refresh values and defaults saved; appearance preserved).
- Real engine with `sl-testkit` (1): `FakePortal` and `FakeDevice` behind the UI. A Sideloadly
  `sessions.json` is imported through the rendered button (one imported, one legacy entry
  skipped); the Apple ID install registers the device, installs `com.example.app.TEAM123456`,
  shows team, bundle, quota and expiry facts and records the installation; "Refresh now" installs
  again, reuses the certificate and produces the engine's refresh notice.
- Unit tests: expiry labels (unknown, days, final-day hours, today, expired days ago) and settings
  URL/number validation. Draft tests add Apple ID/device mapping (account required, policies,
  custom identifier validation, entitlements only for Apple ID, device options only for devices,
  unsigned installs rejected, chunk bounds 1–64).

GPUI 0.2.2 keeps debug bounds from earlier frames, so the tests assert that a control is absent
only before the window has drawn it; later absences are asserted through view state. The test
platform uses a no-op text system, so these tests establish element structure and hit targets,
not text measurement or visual layout.

The native macOS window and attached open/save panels were inspected earlier through computer
use with a generated universal Mach-O app. Native pointer editing and the export shortcut produced
a 3551 B IPA with the requested display name. Python's independent ZIP reader accepted its CRCs
and recorded executable mode 0755. Apple `codesign --verify --strict --deep --all-architectures`
accepted both slices; the executable ran after restoring archive permissions during Python
extraction, and Info.plist tampering was rejected. The source retained its original display name.
Native inspection also covered original-mode disabled fields and light/system appearance;
preference persistence was checked in the isolated fixture directory. The Accounts, Devices,
Installations and Settings sections and the new editor cards have not been inspected natively.

`sl-app` tests, Clippy with warnings denied and formatting pass for this implemented subset.
Live Apple accounts, physical-device installation and pairing, Linux and Windows runtime
behavior, and native rendering of the new sections remain unverified.

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

For `a` accounts, `d` devices, `i` installations and `c` listed certificates, App IDs, apps or
profiles, each render is O(a + d + i + c) elements; account, installation and device lookups by
identifier are linear scans. The view keeps one certificate list and one App ID list (for one
account each), one app and one profile list (for the selected device), and one decoded icon
handle per installation with an icon. Engine-side list sizes are not bounded by the view.

## Assumption register

- A1: Sources remain stable during inspection and export. Dependent results: source fidelity and
  displayed metadata. Probes: source-byte comparison, archive CRC/extent tests, mutation/snapshot
  checks retained in the engine ledger. The view does not create an immutable source snapshot.
- A2: Engine cancellation reaches a checkpoint before commit. Dependent results: destination
  preservation on cancellation. Probes: real pending-prompt close, engine/CLI cancellation during
  packing, and existing-output comparisons. A completed commit is not rolled back by a late click.
- A3: The compatible control/runtime versions provide the tested focus and layout behavior.
  Dependent results: desktop interaction. Probes: rendered hitbox clicks, Enter/Escape tests,
  native open/save flows, and appearance/resize inspection. Arbitrary OS versions are unverified;
  the new sections were exercised only in GPUI's test platform.
- A4: Generated Mach-O fixtures cover the exercised signing cases. Dependent results: fixture
  export/signature verification. Probe: Apple codesign, execution, tamper rejection, and independent
  archive readers. Apple provisioning trust and stock-device acceptance remain unverified.
- A5: Events are consumed while a job runs. Dependent results: practical producer memory.
  Probe: event throttling and bounded view activity; stalled-consumer queue retention is pending.
- A6: A job waiting on a question emits no further events until it is answered, except device
  waits that the engine ends when the device returns. Dependent results: closing device prompts on
  any later event and other prompts on a new stage. Probes: synthetic event ordering in the prompt
  tests and the engine's `select!` between the prompt and device polling; a concurrent log from
  another branch of one job would close a device prompt early (the job then continues or asks again).
- A7: The demo engine and `sl-testkit` fakes model the engine contracts the view consumes.
  Dependent results: account, device, installation and install-flow tests. Probes: the same view
  code against both the demo and the real engine with fakes; live Apple and physical devices are
  not used.

## Bounded observations

- High impact: live Apple sign-in, physical-device installation and pairing, and native rendering
  of the new sections are unverified. AOSKit refused local anisette on the development Mac, so
  real sign-in there needs a remote or alternate provider configured in Settings.
- High impact: Store/private services, tray and other full-scope workflows still require
  implementation and evidence in ARCHITECTURE.md; link sources and autostart are exposed by the
  engine and CLI but not yet by these screens.
- Medium impact: GPUI 0.2.2's native accessibility tree exposes the window/menu rather than these
  custom form controls. Native verification therefore also uses screenshots and rendered control
  tests. Full accessibility integration, non-macOS runtime checks, settings-write latency, and
  strict event backpressure remain pending.
- Medium impact: pairing, uninstall, profile removal, sign-out and import cannot be cancelled by
  the engine API; closing the window abandons their results. Pairing can wait up to 120 s.
- Medium impact: minimum-size scrolling and modal focus matter independently of successful
  exports. Keep rendered/key interaction checks when controls or layout change.
- Low impact: the engine's device watcher can publish its first snapshot before a new subscriber
  is registered; the view therefore also lists devices once when discovery starts.
- Low impact: executable names must differ beyond case on macOS. `SideportDesktop` and `sideport`
  are verified together by workspace CLI subprocess tests.

## Primary sources and gates

- [Zed GPUI source](https://github.com/zed-industries/zed/tree/main/crates/gpui) and the pinned
  [0.2.2 API](https://docs.rs/gpui/0.2.2/gpui/).
- [GPUI Component 0.5.1](https://docs.rs/gpui-component/0.5.1/gpui_component/) and its packaged
  Cargo manifest declaring GPUI 0.2.2 compatibility.
- [RFD source](https://github.com/PolyMeilex/rfd), pinned 0.17.2 AppKit panel ownership and async API.
- [Apple AppKit NSOpenPanel](https://developer.apple.com/documentation/appkit/nsopenpanel).
- Engine contracts: `crates/sl-engine/src/engine.rs`, `types.rs` and `job.rs`; device and
  refresh behavior in [DEVICE.md](DEVICE.md); account and provisioning behavior in
  [APPLE.md](APPLE.md).

QG1–QG7 apply to the implemented subset with the assumptions and verification boundaries above.
The full objective's coverage gate remains open wherever the architecture ledger lists missing work.
