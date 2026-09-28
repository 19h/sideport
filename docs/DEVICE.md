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
data directory run it once (claims older than one hour are taken over), and runs it without
interaction: prompts are declined, so second factors, revocations and retry questions fail the
refresh instead of waiting.

## Device log

`Engine::syslog(udid, filter)` streams `com.apple.syslog_relay` lines (NUL/newline delimited)
as job log events until the job is cancelled; a filter keeps lines containing it,
case-insensitively (the recovered GUI's syslog viewer filters). `sideport device syslog UDID
[--filter TEXT]` prints them until Ctrl-C. On 2026-09-28 the real USB iPhone produced 864 lines in
about eight seconds through the CLI (count only recorded).

## Apple Silicon Mac

On Apple Silicon the device list includes this Mac (`device_class` `Mac`, model name "This
Mac") with the provisioning UDID System Information reports; the recovered `get_m1_udid` helper
reads the same value through MobileGestalt. A device job for that UDID requires Apple ID
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
- Medium impact: Wi-Fi devices depend on usbmuxd's network support; tvOS pairing with a PIN
  (recovered `PairTV`) and the heartbeat service are not implemented.
- Medium impact: unattended refreshes cannot answer prompts; accounts with remembered passwords
  and reusable certificates refresh without interaction.
- Low impact: the recovered `-wifi` UDID suffix is represented by `prefer_network`.
