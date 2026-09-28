# Sideport

Rust reimplementation of Sideloadly 0.60's client behavior, as recovered by reverse
engineering. The full rewrite is in progress. The requirement-by-requirement evidence is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), and the current resume point is in
[docs/HANDOVER.md](docs/HANDOVER.md).

The current executable inspects IPA, flipped IPA, zipped-app, and app-directory inputs.
It exports original, unsigned, and ad-hoc IPAs with recursive metadata edits, extension
removal, local injection, and file replacement. Original archive exports preserve every
input byte. Other operations use a private staging tree and an atomic output transaction.

The GPUI desktop app provides the same inspection, editing, and export flow, with native
file dialogs, stage progress, activity, cancellation, and persistent appearance preferences.

The authentication library implements the recovered SRP/session/token cryptography, a bounded
remote anisette provider, and a GSA client with trusted-device/SMS verification. Controlled
HTTP fixtures cover the GSA operation sequence and retry behavior. A typed developer portal
client covers core team, device, app-ID, certificate and profile actions. The non-demo engine
login job uses remote anisette, attempts team enumeration, and retains its session in memory.
Sessions, remembered passwords and the signing key persist across restarts (keychain on macOS),
and recovered Sideloadly `sessions.json` GSA sessions can be imported. The engine provisions
Apple ID exports: team, certificate reuse or creation, App IDs and trust-verified profiles, then
signs the IPA with the issued identity. The device layer discovers devices through usbmuxd and
installs Apple ID, ad-hoc or original apps with the recovered resumable upload and retry policy,
records installations and refreshes them on a schedule (docs/DEVICE.md). Account UI/CLI and
physical-device acceptance remain open.
Engine/CLI anisette checks use the real transport; see
[docs/APPLE.md](docs/APPLE.md) for independent vectors and the remaining account workflows.

## Desktop

```sh
scripts/cargo-ui.sh run -p sl-app -- MyApp.ipa
scripts/package-macos.sh
```

The packaging command creates `target/debug/Sideport.app` on macOS. Pass `release` to package
an optimized build. The desktop executable is `SideportDesktop`; the CLI remains `sideport`,
so they also coexist on filesystems that ignore filename case.

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
Custom icons currently return an explicit unsupported error.

## Verification

```sh
scripts/cargo-ui.sh test --workspace
scripts/cargo-ui.sh clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Native macOS tests require Xcode command-line tools and use generated fixtures.
See [docs/ENGINE.md](docs/ENGINE.md) and [docs/BUNDLE.md](docs/BUNDLE.md) for verification
boundaries, assumptions, complexity, and remaining work. Source layout follows
[AGENTS.md](AGENTS.md) and [docs/STYLE.md](docs/STYLE.md).
