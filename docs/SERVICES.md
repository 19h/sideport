# Private services implementation evidence

The complete objective remains the workflow ledger in [ARCHITECTURE.md](ARCHITECTURE.md). This
document records `sl-services`, the client side of Sideloadly 0.60's recovered "private services":
its self-update protocol, its BSDIFF40 patcher and its feature-token gating. These services are
operated by a third party; this implementation **does not contact them**, ships **no** endpoints or
keys, and its recovered server behavior and current availability are **unknown**.

## Scope and non-goals

This crate implements protocol *shapes*, not a client bound to any specific server:

- The update protocol is the public `go-selfupdate` + `kr/binarydist` contract (BSDIFF40 patches,
  a JSON manifest, gzip full binaries, SHA-256 verification). The crate carries no base URLs; every
  endpoint, the component name and the platform token are supplied by the caller through
  [`UpdateEndpoints`]. With nothing configured, the engine's update check returns
  `UpdateStatus::NotConfigured` and performs no request.
- The feature-token verifier is a generic RS256 JWT verifier keyed by a **caller-supplied** public
  key, with a Sideport-defined claim schema (below). No verification key and no claim names were
  extracted from the Sideloadly binary.
- The update swap operates only on caller-provided paths. Tests use temporary files. It never reads,
  writes or replaces a real installed binary, and there is no self-update of the running executable.

Deliberately excluded, per the workstream's constraints: any endpoint, key or claim name decoded
from the Sideloadly binary; any behavior that impersonates the Sideloadly client to its backend; and
the HTTP listener for the OAuth return, which lives in the engine's local IPC server. This crate
provides only the services-side function that the `/tokens` route calls with the received token.

The repository contains no `sideloadly.io` endpoints, and this crate does not introduce any.

## BSDIFF40 patcher

`bspatch(old, patch)` reconstructs a file from an `old` image and a BSDIFF40 patch. The format,
recovered from `sideloadlysid/binarydist.Patch`, is a 32-byte header (`BSDIFF40` magic and three
signed-magnitude little-endian int64 lengths: bzip2 control block, bzip2 diff block, new-file size)
followed by the bzip2 extra block. The control block is a run of `(add, copy, seek)` triples: copy
`add` bytes from the diff block added byte-wise to the aligned old bytes, then `copy` literal bytes
from the extra block, then advance the old cursor by `seek`.

The recovered decoder's bounds checks are reproduced: negative header or control lengths, blocks
that exceed the patch, an old cursor outside the old file (which contributes nothing rather than
faulting) and any write past the declared new size are rejected. Truncated or corrupt bzip2 blocks
fail with a patch error.

## Update protocol

`Updater` fetches and verifies one component (`exe`, `lib`, `daemon`, ...) against caller-configured
bases:

- **Manifest** — `GET {info_base}{command}/{platform}.json`. Parsed for a `version` string and a
  base64 `sha256` that must decode to exactly 32 bytes (`Version`/`Sha256` field aliases match the
  recovered Go `versionInfo` JSON). `check(current_version)` compares versions and returns
  `UpToDate` or `Available`.
- **Patch** — `GET {diff_base}{command}/{old}/{new}/{platform}`, a BSDIFF40 patch applied to the
  current binary. The patched bytes must match the manifest SHA-256.
- **Full binary** — `GET {binary_base}{command}/{new}/{platform}.gz`, gzip-decoded and matched
  against the manifest SHA-256.
- **`stage(...)`** tries the patch first and falls back to the full download, writing the verified
  result to a caller-provided staging path (mode 0755 on Unix). The current binary is read, never
  modified. Responses are bounded to 512 MiB after decompression; requests are cancellable.

No cryptographic signature is verified because the recovered updater verifies none — only the
SHA-256 from the manifest. That is a recovered property, recorded here and in the assumptions.

### File swap and the recovered restore bug

`swap::backup(path)` copies a file to `.<name>.bak`. `swap::install(target, staged)` reproduces the
recovered `go-update` atomic replace: rename the live file to `.<name>.old`, rename the staged file
into place, and delete `.old`; a failed final rename rolls `.old` back.

The recovered `updating.CheckAndUpdate` backs the library and daemon up to `.bak` before updating
and, on failure, tries to restore them — but it renames the **daemon** backup onto the **library**
path (`os.rename(daemon.bak, lib_path)`), so a rolled-back library becomes the daemon binary. That
is a recovered bug. `swap::restore_from_backup(path, backup)` does the correct thing: it restores
each file from its own backup.

The recovered client also restarts via a `/bin/sh -c` pipe that waits, kills the old process and
re-execs it. That restart is a host concern and is out of this crate's scope; the update user agent
is generic (`sideport/<version>`), never `sideloadly/<version> darwin`.

## Feature tokens

`TokenVerifier` verifies an RS256 JWT against a caller-supplied RSA public key
(`SubjectPublicKeyInfo`, PEM or DER). It checks the `alg` header is RS256, the RSA PKCS#1 v1.5
SHA-256 signature over `header.payload`, and the `exp`/`nbf` lifetime at a caller-supplied clock.
Only RS256 is accepted; every other algorithm (including `none`) is refused before claims are read.

The claim schema is **Sideport's own**, documented here rather than recovered from the binary. The
recovered client's claim names are hex-obfuscated in the binary and were deliberately not extracted:

```json
{
  "sub": "<optional subject>",
  "exp": 1700003600,
  "nbf": 1699996400,
  "features": {
    "refresh_interval_hours": 6,
    "remote_anisette": true,
    "custom_entitlements": true,
    "custom_icon": true,
    "custom_info_props": true,
    "custom_upload_chunk": true
  }
}
```

`features` is closed (`deny_unknown_fields`): an unrecognized feature key rejects the token rather
than being silently ignored, so a token cannot smuggle capabilities the verifier does not model. The
features mirror the option groups the recovered client gated (a custom auto-refresh threshold, a
private/remote anisette provider, alternate entitlements, a replacement icon, ten-plus custom
Info.plist keys and a custom AFC upload chunk size), but the names and the wire shape are ours.

Decision: Sideport does not gate any option on feature state. The recovered client withheld these
options from users without a paid token; Sideport is a local tool whose options act only on the
user's own files, devices and accounts, so every option stays available and `FeatureState` is
reported (`services status`, the desktop's Settings) but not enforced. An embedder that wants the
recovered gating can check `Engine::services_status().feature_state` before offering an option.

## OAuth return contract

The recovered GUI opens a browser login and serves a one-shot `http://localhost:28811/tokens`
callback that reads a `user_token` query parameter while a login it started is pending, answers with
a fixed success page, and consumes the token once. The HTTP listener is part of the engine's local
IPC server (docs/ENGINE.md, Local IPC). This crate provides `Services::apply_token(token, now)` — the
services-side step: it validates the token with the configured verifier and records the resulting
`FeatureState`, or errors when no verifier is configured. The engine exposes it as
`Engine::apply_feature_token`, and `Engine::receive_feature_token` joins the two: it waits for the
one pending `/tokens` return and verifies the delivered token. An engine test serves IPC, returns a
minted RS256 token through `/tokens` and checks the resulting feature state, and refuses a token
with a bad signature.

## Engine and CLI integration

`EngineConfig::services: Option<ServiceConfig>` supplies the update endpoints, the token public key
(PEM) and the running version. When it is `None` (the default everywhere in this repository), the
engine builds no services client:

- `Engine::services_status()` reports `updates_configured`/`token_verifier_configured` as false and
  no features unlocked.
- `Engine::check_update()` returns `UpdateStatus::NotConfigured` without a request.
- `Engine::apply_feature_token(token)` errors that services are not configured.

The CLI exposes `sideport services status` and `sideport services check-update`. Both reflect the
unconfigured state by default and never contact a network service unless an embedder supplies a
`ServiceConfig`.

## Verification and provenance

`sl-services` has 20 tests: BSDIFF40 diff/copy/mixed reconstruction, an all-copy patch that ignores
the old file, rejection of a wrong magic, a short header, a corrupt block and an over-long control
length, and a cross-check that applies the same generated patches with the platform `/usr/bin/bspatch`
(skipped when that tool is absent — it was present and agreed on 2026-09-28); manifest parsing and
its malformed cases; `check` reporting available/up-to-date; `stage` preferring the patch, falling
back to the full binary, rejecting a full binary that fails its checksum, and stopping when already
cancelled (all against a `wiremock` server); the `.new`/`.old`/`.bak` swap with restore; RS256 token
verification of subject/expiry/features, rejection of expired, not-yet-valid, foreign-key, tampered,
wrong-algorithm and malformed tokens, and rejection of an unknown feature claim; and the empty-config
paths (nothing configured, `NotConfigured`, no-verifier error). A CLI subprocess test confirms
`services status` and `services check-update` report the unconfigured state.

Primary sources recovered by reverse engineering:

- `sideloadly/updating`, `sideloadly/updating/fetch`, `sideloadly/updating/restart` and the embedded
  `go-selfupdate`/`go-update` fork: the manifest/patch/full-binary URL layout, SHA-256 verification,
  the `.new`/`.old`/`.bak` swap and the daemon↔library restore bug.
- `sideloadlysid/binarydist.Patch`: the BSDIFF40 signed-magnitude header and control encoding.
- `sideloadly/gui/patreon` and `sideloadly/gui/ipc`: the browser-login + `/tokens` one-shot callback
  and the RS256 feature-token gating (the concept; no key or claim names are reproduced).
- The public [`kr/binarydist`](https://github.com/kr/binarydist) and
  [`go-selfupdate`](https://github.com/sanbornm/go-selfupdate) libraries: the format the fork tracks.
- [`bsdiff`/`bspatch`](https://www.daemonology.net/bsdiff/) (Colin Percival): the BSDIFF40 algorithm.

## Assumptions

- S1: The recovered updater tracks the public `go-selfupdate`/`binarydist` format. Dependent result:
  a real update server serving that format would be handled. Probe: generated fixtures and the
  system `bspatch` cross-check; a live server trace would confirm the URL layout. Current
  availability is unknown.
- S2: The recovered updater verifies only the manifest SHA-256, with no code signature. Dependent
  result: an operator configuring these endpoints must trust the manifest's transport (HTTPS) and
  server. Probe: recorded here; a signed-update variant is out of scope until evidence requires it.
- S3: The recovered feature token is an RS256 JWT. Dependent result: RS256 verification suffices for
  a compatible token. Probe: token fixtures with a generated key. The recovered signing key and claim
  names were not extracted; the schema above is Sideport's own and any real key/claims would be
  supplied by configuration.
- S4: The `/tokens` return delivers a `user_token` to a one-shot listener. Dependent result: the
  engine's IPC route can hand that token to `apply_token`. Probe: the engine's IPC tests deliver a
  minted token through the real listener into feature state; this crate validates the
  token-to-feature-state step.

## Bounded observations

- High impact: mock/fixture success establishes client encoding and parsing only. It does not
  establish that any real update or token server exists, is reachable, or would accept these
  requests; that status is unknown until a dated live check with an operator-supplied endpoint.
- High impact: the recovered update path verifies no signature. Configuring update endpoints trusts
  the manifest and its transport; this is recorded, not silently accepted.
- Medium impact: the BSDIFF40 decoder's bounds checks are exercised by generated fixtures and the
  system `bspatch`; arbitrary real patches from an unknown encoder are not covered.
- Low impact: grouped code layout must survive formatting; the crate is reviewed under
  [AGENTS.md](../AGENTS.md) and [STYLE.md](STYLE.md).
