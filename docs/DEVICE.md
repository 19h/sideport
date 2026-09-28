# Device transport and installation evidence

The complete objective remains the ledger in [ARCHITECTURE.md](ARCHITECTURE.md). This document
records `sl-device` and the engine's device jobs. The recovered client behavior comes from
Sideloadly 0.60's `sideloadly.mobdev` and `sideloadly.impact` modules (libimobiledevice via
CFFI), recovered by reverse engineering. Physical installation has not been performed.

## Layers

- `sl_device::mux`: usbmuxd discovery through `idevice`. usbmuxd lists each transport
  separately; `Mux::attached` groups entries by UDID with USB first. `Mux::provider` selects USB
  unless the job prefers the network and a network entry exists (the recovered
  `IDEVICE_LOOKUP_USBMUX | NETWORK [| PREFER_NETWORK]`). The lockdown label is `sideport`
  (recovered `sideloadly`). The listen stream is not `Send`, so it runs on a dedicated thread
  with a current-thread runtime and stops when its receiver is dropped.
- `sl_device::install`: the recovered upload and retry policy, written against the `Connector`,
  `Session`, `Staging` and `Package` traits.
- `sl_device::backend`: `idevice` implementations. AFC staging keeps one append handle open.
  Installation-proxy messages are framed directly (32-bit big-endian length and plist) over the
  service socket, because `idevice`'s client reduces errors to a description; the recovered
  policy needs `Error`, `ErrorDescription` and `ErrorDetail`. Apps come from `Browse`
  (`ApplicationType=User`), profiles from misagent `CopyAll`, and pairing uses lockdown `Pair`
  followed by usbmuxd `SavePairRecord`.
- Engine `devices` and `sideload`: snapshots, a watcher that republishes after attach/detach
  events and reconnects every 5 s, device utilities, and install jobs.

## Installation contract

Each attempt connects lockdown, AFC and installation proxy. The package path is
`PublicStaging/<bundle id>`. The first attempt removes a previous upload; retries keep it.
`StagingDirectory` is removed (errors ignored) and `PublicStaging` created.

Upload: a staged file larger than the package is removed and the attempt retried (recovered
`FileTooBig`); an equal one is kept; otherwise bytes are appended from the staged size in exact
chunks (default 1 MiB, `upload_chunk_mib` 1–64). After upload the staged size must equal the
bytes produced; a mismatch removes the file and retries. Installation sends
`{Command: Install, PackagePath, ClientOptions: {CFBundleIdentifier}}` and reads status messages
until `Complete`.

Retries: an attempt that fails with an interrupted connection, a size problem, an extraction
failure (`Could not extract archive`) or an installation failure that leaves the staged package
retries automatically. After four automatic retries the user is asked whether to continue. A
vanished device is retried at once if attached again, else polled once per second for 180 s,
then the user is asked (`WaitForDevice`), with the engine continuing if the device returns.
Pairing states (passcode, trust declined, trust pending) ask the user with the connection
reason. Other errors stop.

Streaming: with `stream_upload`, the prepared archive is packed on a blocking worker at
compression level 1 (recovered `ZIP_COMPRESSION = 1`) into 256 KiB blocks on an 8-deep bounded
channel, so at most about 2 MiB of generated ZIP waits for the device. A resumed upload
regenerates the deterministic stream and discards the staged prefix; a stream shorter than the
staged file is treated as `FileTooBig`. Progress uses an upper estimate (uncompressed payload
plus per-entry overhead) until the stream ends.

Deliberate differences from the recovered client:

- installd errors that retrying cannot fix (`ApplicationVerificationFailed`,
  `DeviceOSVersionTooLow`, `IncorrectArchitecture`,
  `MismatchedApplicationIdentifierEntitlement`, `BundleValidationFailed`) stop immediately; the
  recovered client retries every error while the staged file exists.
- declining "There was an issue during installation" fails the job; the recovered client
  swallows the error and reports success.
- the device-wait loop stops polling when the device returns; the recovered loop continues.
- unsigned bundles cannot be installed (export only), matching `SigningMode::Unsigned`.

## Engine jobs

`Target::Device` inspects the input, reads lockdown values, and signs according to the mode:
Apple ID (provisioning registers the device and requires it in every embedded profile), ad-hoc,
or original. An original plain IPA is uploaded unchanged; flipped or directory inputs are
repacked. With `track_for_refresh` and Apple ID signing, the input is copied to
`<data dir>/files/<sha256>.ipa` and the installation recorded (unique per device and bundle ID;
a reinstall keeps its row and automatic-refresh choice). `Engine::refresh` replays the stored job
and records success or the failure count. The scheduler (disabled by
`EngineConfig::disable_scheduler`) runs every `check_interval_minutes`, queues automatic
installations expiring within `threshold_hours` whose paired device is attached over USB (or
the network when `allow_network`), claims each queue entry in the database so processes sharing a
data directory run it once, and runs it without interaction: prompts are declined, so second
factors, revocations and retry questions fail the refresh instead of waiting. A claim older than
one hour (a crashed process) is taken over, and so is one more than five minutes in the future
(the clock was set back since it was written); the crashed or displaced process can no longer
clear the entry. Due selection compares expiry with the current clock, so a clock set forward makes
installations due at once and one set back defers them. Store and selection fixtures cover the
crash takeover, both clock directions and the five-minute tolerance. A real-process test kills
a `sideport refresh-due` process while it holds a claim, shows that another pass within the hour
leaves the claim alone, then ages the claim and shows the next pass takes the entry over, runs
it and records the failure (`crates/sl-cli/tests/processes.rs`). The desktop app's local IPC also queues a refresh through
`/enqueue` (docs/ENGINE.md).

## Device log

`Engine::syslog(udid, filter)` streams `com.apple.syslog_relay` lines (NUL/newline delimited)
as job log events until the job is cancelled; a filter keeps lines containing it,
case-insensitively (the recovered GUI's syslog viewer filters). `sideport device syslog UDID
[--filter TEXT]` prints them until Ctrl-C. On 2026-09-28 the real USB iPhone produced 864 lines in
about eight seconds through the CLI (count only recorded).

## Apple Silicon Mac

On Apple Silicon the device list includes this Mac (`device_class` `Mac`, model name "This
Mac") with the provisioning UDID read directly through MobileGestalt, as the recovered
`get_m1_udid` helper does. System Information reports the same value. A device job for that UDID requires Apple ID
signing. Provisioning registers the Mac as a device and, as the recovered client does for Apple
Silicon, mangles free-team identifiers regardless of OS version. The signed app is written as
`<tmp>/sideport-m1-<uuid>/Payload/<App>.app`, then converted (recovered `m1ConvertAndInstall`):
`Payload` → `Wrapper`, a relative `WrappedBundle` → `Wrapper/<App>.app` link, the executable made
0755, and `sideloadly.tag` holding the installation token. Placement in the applications
directory (default `/Applications`) replaces an application whose tag holds the same token
(wherever the user renamed it), else uses `<display name>.app` with `/` replaced by `_`,
overwriting only an application whose wrapped bundle has the same identifier and otherwise
choosing `<name>-<n>.app`. Tracked installations use a stable token derived from the bundle ID,
so refreshes replace their application; one-off installs write an empty token. The outcome's
`exported_to` is the installed application path.

Differences: the recovered client chmods `Wrapper/<App>.app/<App>` (assuming the executable
is named after the app); Sideport uses `CFBundleExecutable`. Its token is a random UUID stored
with the installation; Sideport derives it from the bundle ID. Launching the installed app has
not been verified; it needs an Apple-issued development identity whose profile lists the Mac.

## Device utilities

These run behind the `Backend` trait so a fake device exercises them without hardware
(`crates/sl-testkit/src/device.rs`). Sources: reconstruction §8.1/§12, `notes/MOBDEV_NOTES.md`,
`decompiled/go/sideloadly_mobdev.c` and `daemon_main.c`.

- **Developer Disk Images** (`sl_device::ddi`, `mounter`, `tss`). `Catalog` resolves an image for
  the device's `ProductVersion` from the recovered GitHub mirrors: the `releases/tags/<version>`
  asset of `xushuduo/Xcode-iOS-Developer-Disk-Image` and `mspvirajpatel/Xcode_Developer_Disk_Images`
  (extracting the `.dmg` and `.dmg.signature` from the zip, ignoring `__MACOSX`), else
  `pdso/DeveloperDiskImage/master/<version>/DeveloperDiskImage.dmg` (+ `.signature`) directly.
  Version selection tries the exact version then `major.minor` (recovered `getImageForVersion`).
  `Store` caches into `<data dir>/developer-disk-images/devimg-<major.minor>.dmg` (+ `.signature`).
  `IsMounted` is a `LookupImage` for `Developer` then `Personalized`; an already-mounted image
  short-circuits. Legacy mount uploads `Developer` and mounts it with the signature.
  **Personalized DDI (iOS 17+)**: the image, `Image.dmg.trustcache` and `BuildManifest.plist` come
  from `doronz88/DeveloperDiskImage`; the signature is the device's own personalization manifest
  when it has one, else a TSS ticket. `TssClient` POSTs to `http://gs.apple.com/TSS/controller?action=2`
  (endpoint configurable) with the recovered request — `@HostPlatformInfo`/`@VersionInfo`
  (`libauthinstall-973.0.1`)/`@UUID`/`@ApImg4Ticket`/`@BBTicket`, `ApBoardID`/`ApChipID`/`ApECID`
  from `QueryPersonalizationIdentifiers`, `ApNonce` from `QueryNonce`, a 20-byte zero `SepNonce`,
  the production/security flags the device reports, and the build-manifest components with their
  `LoadableTrustCache` `RestoreRequestRules` applied — then parses `STATUS=0&…&REQUEST_STRING=` and
  extracts `ApImg4Ticket`. `idevice`'s `select_build_identity`/`apply_restore_request_rules`/
  `extract_img4_ticket` do the manifest math. All downloads are cancellable; endpoints are
  configurable through `EngineConfig::ddi` so tests use wiremock and never contact GitHub or Apple.
- **JIT** (`sl_device::jit`, "Enable JIT for Apps"). `Engine::enable_jit` mounts the developer
  image, reads the app's bundle path/container/`CFBundleExecutable` from the installation proxy,
  and drives debugserver over the recovered lockdown path (`com.apple.debugserver.DVTSecureSocketProxy`
  then `com.apple.debugserver`). Launch sends `QSetLogging`, `QSetMaxPacketSize:1024`,
  `QSetWorkingDir:<container>`, the `A` set-argv packet with the bundle path, `qLaunchSuccess`;
  attach sends `vAttachOrWait;<hex exe>`; both end with `D` (detach) so the app keeps running with
  debugging (and thus JIT) enabled, exactly as recovered `StartJIT`.
- **Pairing repair** (`Engine::repair_pairing`). Unpairs (lockdown `Unpair` + usbmuxd
  `DeletePairRecord`), then pairs again with the trust dialog, emitting typed progress the GUI can
  drive (recovered `RepairPairing`).
- **Heartbeat and notifications**. `Engine::heartbeat` runs one `Marco`/`Polo`; the interval proves
  a network or tvOS device is reachable. `Engine::notifications` forwards `notification_proxy`
  events (default `com.apple.mobile.application_(un)installed`) as job log events until cancelled.
- **Wi-Fi devices** already surface in the device list with their connection kind (`Connection::Network`).

CLI: `device mount-ddi UDID`, `device jit UDID BUNDLE_ID [--attach]`, `device repair-pairing UDID`,
`device heartbeat UDID`, `device notifications UDID [--name NAME]`.

Deliberate deviations:

- The personalized TSS request copies the device's `Ap,*` identity tags (as `idevice` does); the
  recovered client parses only the typed identifier fields, discarding `Ap,*`. It also uses the
  production/security flags the device reports (recovered `CertificateProductionStatus`/
  `CertificateSecurityMode`) rather than hard-coding them.
- JIT is the pre-iOS-17 lockdown debugserver path only, matching the recovered client. iOS 17+
  moves debugserver behind the RSD/CoreDevice tunnel, which needs a root network tunnel; the
  recovered client has no such path, so it is unimplemented.
- **tvOS PIN pairing** (recovered `PairTV`, lockdown `cu_pairing`/`pair_cu` with an SRP PIN
  exchange) is **not** implemented: `idevice` 0.1.68 exposes no CU-pairing API. It remains open.
- A `QSetWorkingDir` error is ignored and JIT continues (recovered code ignores error code 60).

## Verification

Eleven fault-injection tests run the policy against a fake device: clean install order and
monotonic progress; a write interrupted mid-chunk resumes without gaps or duplicates; oversized
and short staged files are removed and uploaded again; five installation failures with a staged
package (one attempt plus four automatic retries) then a continue question; terminal installd
errors stop at once; installation issues ask before retrying and extraction failures retry
automatically; a vanished device is polled and the upload resumes; a device that does not return
leads to the prompt after 181 checks; pairing states ask with the reason; cancellation stops
before installation leaving a resumable prefix; a generated stream resumes by skipping the staged
prefix. Four engine tests use the fake portal and a fake device layer: Apple ID installation with
device registration, the UDID in the embedded profile, the installation record and stored copy,
refresh reusing the certificate while keeping the refresh choice, and forgetting that deletes the
stored copy; ad-hoc and byte-identical original installs without an account; an interrupted
streamed upload whose resumed bytes equal a clean stream; and device values, profiles, apps,
uninstall, pairing and a detached-device error. An Apple Silicon test registers a fixed Mac
UDID, checks the profile, wrapper, link and tag in a temporary applications directory, refreshes
into an application the user renamed, places a one-off install by name, and refuses ad-hoc
signing. The CLI lists this Mac by its computer name through the real system query. Store tests cover schema migration from version
1 and refresh claims, including takeover of a stale claim.

Seven `sl-device` DDI tests cover the catalog and mount flow against wiremock and a fake image
mounter: a legacy image downloads from the raw mirror and is served from the cache afterwards;
resolution falls back from `16.5.1` to `16.5`; a legacy mount uploads the `Developer` image when
none is mounted; an already-mounted image short-circuits; a personalized mount uses the device's
own manifest without contacting TSS; a personalized mount fetches a TSS ticket (via a wiremock
`STATUS=0&…&REQUEST_STRING=` response) when the device has none, in the recovered call order; a
cancelled download stops. Ten `sl-device` unit tests add the TSS request build (recovered tags and
`RestoreRequestRules` application), response parsing, version selection, zip extraction and the JIT
hex-encoding/response handling. Three engine tests use the fake device layer: mounting a legacy
image downloads through wiremock and uploads it; enabling JIT (with an image already mounted)
produces the exact recovered debugserver command sequence; and repairing pairing unpairs then
pairs again with typed progress.

On 2026-09-28 an ignored read-only probe (`cargo test -p sl-device --test probe -- --ignored`)
listed one USB device through the system usbmuxd, opened a paired lockdown session and read
`ProductType iPhone17,2`, `ProductVersion 27.2`, device class and name, and opened AFC and
installation-proxy sessions and found `PublicStaging`. The same day, `sideport devices`,
`device apps` and `device profiles` read the device through the real engine: 146 user apps and
two installed profiles (counts only recorded). A signer histogram showed App Store apps signed by
`Apple iPhone OS Application Signing` without `ProfileValidated`, and development apps with a
developer signer and `ProfileValidated = true`; `is_developer_app` therefore means "not signed by
the App Store signer" (2 of 146). These probes wrote nothing. No application was installed on a
physical device.

On a later date the read-only probe additionally connected the image mounter and queried whether a
developer image was mounted: on an `iPhone17,x`/iOS 27.x device the mounter answered reachable with
no developer image mounted (shape only recorded; the query uploads and mounts nothing). No image was
mounted, no debugserver launched, no pairing changed on the physical device — those need explicit
permission that was not given.

## Assumptions

- D1: `idevice` 0.1.68 implements usbmuxd, lockdown TLS, AFC and pairing compatibly with current
  devices. Dependent results: every physical operation. Probe: the read-only probe above; physical
  installation, Wi-Fi and tvOS remain unverified.
- D2: installd reports `Error`, `ErrorDescription` and `ErrorDetail` and ends with
  `Status=Complete`. Dependent result: retry classification. Probe: framed-message decoding and
  fake-device fixtures; compare real failure messages during physical checks.
- D3: The packer is deterministic across runs. Dependent result: stream resume. Probe: engine
  stream resume equality and sl-bundle determinism tests.
- D4: The system clock and database claims order refreshes. Dependent result: one refresh per
  queue entry across processes. Probe: claim and stale-claim fixtures; crash tests remain.

## Bounded observations

- High impact: fixtures establish the installation contract, not acceptance by installd;
  physical installation with an authorized development identity is required.
- Medium impact: Developer Disk Image mounting, JIT and pairing repair are established by
  fixtures against a fake image mounter/debugserver, not by mounting or launching on a real
  device; personalized DDI depends on Apple's live TSS controller (never contacted in tests).
- Medium impact: Wi-Fi devices depend on usbmuxd's network support; the heartbeat and
  notification services are implemented but verified only against the fake device. tvOS PIN
  pairing (recovered `PairTV`, lockdown CU pairing) is not implemented — `idevice` 0.1.68 has no
  CU-pairing API — and remains open.
- Medium impact: unattended refreshes cannot answer prompts; accounts with remembered passwords
  and reusable certificates refresh without interaction.
- Low impact: the recovered `-wifi` UDID suffix is represented by `prefer_network`.
