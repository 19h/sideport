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
cargo run -p sl-cli -- inspect MyApp.ipa --json
cargo run -p sl-cli -- export MyApp.ipa --output Prepared.ipa --signing ad-hoc \
    --bundle-id com.example.prepared --name "Prepared App" --file-sharing
cargo run -p sl-cli -- export MyApp.ipa --output Unsigned.ipa --signing unsigned \
    --remove-extension Widget.appex --inject ./libExample.dylib
cargo run -p sl-cli -- export MyApp.ipa --output Original.ipa --signing original
cargo run -p sl-cli -- export --help
cargo run -p sl-cli -- anisette --remote http://127.0.0.1:6969 --json
```

Pass `--output` for unattended jobs. Interactive exports use a save-path prompt.
Ctrl-C cancels the worker and waits for it to release its output transaction. A cancellation
that reaches a checkpoint before commit leaves an existing destination unchanged.

`--set`, `--set-bool`, `--set-integer`, and `--remove-key` provide typed Info.plist edits.
`--replace TARGET=SOURCE` copies a file/directory into the prepared app;
`--remove-file TARGET` removes it. Paths are relative to the app root.
`run SPEC.json` consumes the engine's serialized `JobSpec` contract.

Inspection JSON omits decoded icon bytes. Inspection does not verify unvisited archive
payloads. Ad-hoc signing does not provision an app for stock-device installation.
Custom icon and entitlement edits currently return an explicit unsupported error.

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
