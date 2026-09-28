# Acquisition channel evidence

The complete objective remains the ledger in [ARCHITECTURE.md](ARCHITECTURE.md). This document
records `sl-acquire`, recovered from Sideloadly 0.60's Go `sideloadly/urischeme`,
`sideloadly/updating/fetch` and `sideloadly/ipa` packages by reverse engineering. Endpoint
availability of any real link source is unknown.

## Links

`Link::parse` accepts only the `sideloadly` scheme ("Not a valid Sideloadly URL"). An opaque
form `sideloadly:<url>` downloads the opaque part plus its query. With an empty path, the query
supplies `dn` (display file name), `xs` (IPA URL), `h` (hex MD5 or SHA-1; other lengths or bad
hex are "Improper Sideloadly URL"), `metadata`, `sinfs` and `artwork`. Without `xs` the link is an
App Store deeplink that requires a known `c` country (case-insensitive) and `bi`, with optional
`v` ("Incorrect Sideloadly URL"). Any non-empty path is "Invalid Sideloadly URL"; unparsable text
is "Bad URL". File names follow Go `path.Base` of the URL path, the host when that is `/` or `.`,
and `<bundle id><version>.ipa` for deeplinks. The 155 country codes and names were read from the
recovered binary's `appstore/countries` table; its third field is zero in every entry, so
storefront identifiers come from Store responses.

## Downloads

Each attempt resumes from the destination length with `Range: bytes=<n>-` and accepts 200 or
206. A failed attempt that copied bytes continues immediately with the delay reset to 1 s; one
that copied nothing waits twice the previous delay (2, 4, 8, 16 s) and gives up, deleting the
file, when the next wait would reach 20 s. `text/html` responses fail without retrying and quote
at most 256 bytes of the body. A fresh download must begin with `PK\x03\x04` ("Not a valid IPA
file!", not retried). After the body, the MD5 or SHA-1 digest is verified and, when metadata is
present, the IPA is enriched; either failure truncates the file and downloads again.

Enrichment copies every entry unchanged and adds `iTunesMetadata.plist` (metadata text verbatim),
`Payload/<app>.app/SC_Info/<app>.sinf` (base64 SINF, named after the app directory) and
`iTunesArtwork` (fetched from the artwork URL, bounded to 16 MiB). Downloads are stored XOR-0xAA
flipped as `sideloadly-<uuid>.ipa` under `<data dir>/downloads`, which `sl-bundle` reads
directly. A job's download is removed when the job ends; tracked installations keep their own
content-addressed copy.

Deliberate differences: a resume answered with 200 (the server ignored `Range`) restarts the
file instead of appending a second copy; rejected complete downloads (digest or enrichment) are
retried at most three times instead of indefinitely; the user agent is `sideport/<version>`
instead of `sideloadly/<version>@darwin`. App Store deeplinks are parsed but reported as
unsupported, and the Store client is excluded from this implementation: the recovered client
impersonates Apple's iTunes client, authenticates with kbsync client-attestation tokens produced
through the Mail plug-in (which it disables on macOS Sonoma and later), and downloads
FairPlay-protected packages with their decryption metadata.

## Remote and special injection sources

`special` resolves injection items that are not local files, reconstructed from the Go
`slpy.(*injector)`. Every endpoint lives in a configurable `SpecialSources` struct (default: the
recovered public hosts) so tests point it at wiremock; the real hosts are never contacted in
tests. `SpecialResolver` owns an HTTP client and the sources so callers need no `reqwest` types.

* `///special/substrate` scrapes the MobileSubstrate version from the text between `latest">` and
  the next `<` on the Cydia package page (default `0.9.6301` on any failure) and formats
  `.../mobilesubstrate_<v>_iphoneos-arm.deb`.
* `///special/substitute` scans the Bingner directory listing for `com.ex.substitute_*.deb`,
  picks the highest numeric version (default `com.ex.substitute_2.1.0_iphoneos-arm.deb`) and joins
  it onto the index URL.
* `///special/spoofer` reads the `filename` field of `spoofer.json` and joins it onto the base.

`download` streams an arbitrary artifact (a `.deb` or tweak) to a path with cancellation and a
512 MiB bound, making no IPA assumptions (no flip, ZIP-magic check or hashing), unlike
`Downloader`. The engine downloads these into `<data dir>/injection-cache`, unpacks `.deb`s with
`sl_bundle::extract_deb` and removes the cached files when the job ends; see docs/BUNDLE.md.

## Verification

Six unit tests cover opaque and query links, digests, deeplinks, recovered error messages, the
Go-style file names and the country table. Seven transport tests run against a local HTTP
server: flipped storage with progress and user agent; resume with `Range` and restart when the
server ignores it; HTML and non-ZIP rejection with one request each; a digest mismatch that
downloads again, and a persistent mismatch that stops after three downloads; backoff on 503 that
makes four requests with a 10 ms/100 ms schedule and deletes the file; enrichment of metadata,
SINF and artwork; and cancellation within 100 ms of a 5 s response delay. An engine test exports
a verified link source, removes the download, returns a cached download, reports a hash mismatch
and refuses a deeplink.

Seven `special` tests run against wiremock: substrate version scraping and its default fallback,
substitute picking the highest listed version, the spoofer filename, an unknown special, a plain
file download, and version-tuple ordering. An engine test resolves `///special/substrate` against
a wiremock host and injects the resolved deb's dylib.

## Assumptions

- Q1: Link producers use the recovered parameter names and encodings (metadata as plist text,
  SINF as standard base64). Probe: fixtures above; compare real links when available.
- Q2: Servers honor `Range` or send the whole file with 200. Probe: both cases above.
- Q3: Enriched entries match what the recovered client writes. Probe: entry names and bytes;
  device acceptance of enriched FairPlay apps is outside these fixtures.
