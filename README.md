# Sideport

Rust reimplementation of Sideloadly 0.60's client behavior, as recovered by reverse
engineering. The full rewrite is in progress. The requirement-by-requirement evidence is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), and the current resume point is in
[docs/HANDOVER.md](docs/HANDOVER.md).

The engine inspects IPA, flipped IPA, zipped-app and app-directory inputs, edits metadata,
removes extensions, injects local, remote, special and `.deb` tweaks, replaces files and icons,
and signs children before parents in original, unsigned, ad-hoc or Apple ID mode. Apple ID
signing signs in through GrandSlam (with AOSKit or remote anisette), provisions the team,
certificate, device, App IDs and trust-verified profiles, and exports an IPA or installs it. The
device layer discovers devices through usbmuxd and installs with the recovered resumable upload
and retry policy, mounts Developer Disk Images, enables JIT, repairs pairing, lists and removes
apps and profiles and streams the syslog. Installations are recorded and refreshed on a
schedule by the CLI daemon or the `sideport-tray` menu-bar daemon, and the desktop app serves
the recovered local IPC. `sideloadly:` links and HTTP(S) IPA URLs are job sources. Private
Sideloadly services are only modelled (`sl-services`, nothing configured); the App Store client
is excluded (docs/ACQUIRE.md).

Every output is written through a private staging tree and an atomic commit; jobs fail rather
than write output when an input changes while they read it. Physical-device installation and
live Apple-account use have not been verified yet; see [docs/HANDOVER.md](docs/HANDOVER.md).

## Desktop

```sh
scripts/cargo-ui.sh run -p sl-app -- MyApp.ipa
scripts/package-macos.sh
```

The packaging command creates `target/debug/Sideport.app` on macOS. Pass `release` to package
an optimized build. The desktop executable is `SideportDesktop`; the CLI remains `sideport`,
so they also coexist on filesystems that ignore filename case. The bundle also contains
`sideport` and `sideport-tray` (the login item's scheduler) and registers the `sideloadly` URL
scheme and IPA documents.

On macOS, the wrapper supplies the active SDK to GPUI's bindgen build. Xcode and its Metal
toolchain are required; `xcodebuild -downloadComponent MetalToolchain` installs the component
when Xcode reports it missing. The interface uses Zed's official `gpui` 0.2.2 package and
the compatible `gpui-component` 0.5.1 controls. See [docs/UI.md](docs/UI.md) for scope and evidence.

## Commands

```sh
sideport inspect MyApp.ipa --json
sideport export MyApp.ipa --output Prepared.ipa --signing ad-hoc \
    --bundle-id com.example.prepared --name "Prepared App" --file-sharing
sideport export MyApp.ipa --output Unsigned.ipa --signing unsigned \
    --remove-extension Widget.appex --inject ./libExample.dylib

sideport settings anisette --remote https://anisette.example
sideport account login jane@example.com --remember
sideport account import            # Sideloadly's sessions.json
sideport devices
sideport install MyApp.ipa --device 00008030-001A2D0C0E38802E --apple-id jane@example.com --track
sideport export MyApp.ipa --signing apple-id --apple-id jane@example.com --output Signed.ipa

sideport certificates jane@example.com [--revoke SERIAL]
sideport app-ids jane@example.com
sideport device apps|profiles|pair|uninstall|remove-profile UDID ...
sideport installations
sideport installation refresh|forget|auto-refresh ID ...
sideport refresh-due               # one scheduler pass (LaunchAgent/cron)
sideport daemon                    # keep the refresh scheduler running
sideport-tray                      # the same scheduler behind a menu-bar icon (macOS, Windows)
sideport device mount-ddi|jit|repair-pairing|heartbeat|notifications UDID ...
sideport account default-team jane@example.com TEAMID|--ask
sideport ipc raise [--open FILE] | enqueue ID | poll | restart
sideport settings autostart --enable
sideport services status           # private-service/feature state (nothing configured by default)
sideport services check-update     # version check against a configured endpoint, if any
```

Private-service clients (a BSDIFF40 patcher, the update protocol and a feature-token verifier) live
in `sl-services`. They ship no endpoints or keys and contact nothing unless an embedder supplies a
configuration; see [docs/SERVICES.md](docs/SERVICES.md).

Prompts (password, verification code or `sms`, team choice, confirmations, device
reconnection) are asked on the terminal. Without a terminal they are declined, so unattended
runs fail instead of waiting. Ctrl-C cancels the worker and waits for it to release its output
transaction. `--password-stdin` reads a password for `account login`. `--json` prints results to
stdout; diagnostics go to stderr.

`--set`, `--set-bool`, `--set-integer`, and `--remove-key` provide typed Info.plist edits.
`--replace TARGET=SOURCE` copies a file/directory into the prepared app;
`--remove-file TARGET` removes it. Paths are relative to the app root.
`run SPEC.json` consumes the engine's serialized `JobSpec` contract.

Inspection JSON omits decoded icon bytes. Inspection does not verify unvisited archive
payloads. Ad-hoc signing does not provision an app for stock-device installation.
`--icon PNG` replaces loose app icons in re-signing modes; asset-catalog icons are not rewritten.

## Verification

```sh
scripts/cargo-ui.sh test --workspace
scripts/cargo-ui.sh clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
scripts/test-linux.sh        # non-desktop suite on x86_64 Linux, offline (Docker, cargo-zigbuild)
```

Native macOS tests require Xcode command-line tools and use generated fixtures. Tests never
contact the network except local fixture servers.
See [docs/ENGINE.md](docs/ENGINE.md) and [docs/BUNDLE.md](docs/BUNDLE.md) for verification
boundaries, assumptions, complexity, and remaining work. Source layout follows
[AGENTS.md](AGENTS.md) and [docs/STYLE.md](docs/STYLE.md).
