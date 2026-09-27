# Apple authentication and portal implementation evidence

The complete objective remains the workflow ledger in [ARCHITECTURE.md](ARCHITECTURE.md).
This document records the authentication and portal clients and their verification boundaries.
Live Apple authentication and the current availability of the recovered services are unknown.

## Requirement coverage

| Recovered requirement | Implementation/evidence | Remaining evidence or work |
|---|---|---|
| Remote anisette GET, user hash, time refresh and caching | `sl-apple::anisette`; actual HTTP mock, query preservation, shared refresh, user change and clock/bucket tests | Authorized live provider verification; private provider/feature-token integration |
| Local AOSKit, Mail/AltServer notification protocol and kbsync | Not implemented | Native bridge, bounded IPC, fallback selection and native verification |
| SHA-256 SRP, 2048-bit group, `s2k`/`s2k_fo`, M1/M2 | Consuming `SrpClient` → `SrpProof` → `VerifiedSession`; eight independent Python vectors; GSA init/complete/apptokens over bounded XML plist HTTP with cookie scoping; engine login job | Authorized live GSA account verification |
| Negotiation proof, session-data CBC and app-token GCM | HMAC verification precedes CBC; strict PKCS#7; authenticated `XYZ` token envelope; independent CBC/GCM vectors and complete mock GSA exchanges | Token persistence and live service verification |
| Alternate anisette retry on -36607 | Complete-operation mismatch switches providers once and restarts the exchange; mock server verifies selection and bound; engine settings accept an alternate remote provider | UI controls and live verification |
| Trusted-device/SMS 2FA, repair/security-upgrade handling | Client prompts through `FactorDelegate`; trusted-device/SMS transport, code validation, bounded retry and one login restart are exercised against a mock server; repair/upgrade return typed errors; engine prompt bridge | Live parity for uncertain recovered branches, desktop account controls and account verification |
| Legacy IDMS, session migration/persistence | Engine holds successful GSA sessions in memory; legacy IDMS and session migration are not implemented | Legacy client, keychain/storage, portal validity and migration implementation |
| Portal teams/devices/certificates/app IDs/profiles and free/paid/tvOS policy | Typed QH65B2 client for listing/creating devices and app IDs, listing/submitting/revoking development certificates, and downloading profiles; mock-server paths, headers, schema, tvOS and limits; engine enumerates teams, lists certificates and app IDs, and handles explicit certificate revocation | Live portal compatibility, certificate/profile/device provisioning policy, remaining actions and UI integration |
| Store/FairPlay/kbsync and private services | Not implemented | Client implementations, controlled fixtures and authorized live verification |

`AuthClient::login` accepts remote anisette providers and a factor delegate. Its HTTP fixtures
exercise request fields, cookies, proofs, factor paths and failure transitions; they do not
establish acceptance by Apple's current servers. The non-demo `Engine::login` job now maps
password/second-factor prompts into this client and holds completed sessions in memory. It
requires remote anisette configuration, attempts portal team enumeration after GSA, and leaves
teams empty with a warning if that call fails. It rejects password-remember requests until
secure storage is implemented. `Engine::test_anisette` runs the real provider on the engine's
Tokio runtime.
`sideport anisette --remote URL [--json]` reports its machine description. Neither interface
prints the OTP headers. Desktop and CLI account controls remain to be integrated.

## Cryptographic byte contract

Use SHA-256, the RFC 5054 Appendix A 2048-bit group and generator 2. The client ephemeral
contains 32 random bytes with its most significant bit set, consistent with the recovered
fork's 32-byte constants. The independent upstream implementation accepts a 256-byte
ephemeral buffer; fixtures left-pad the same 32-byte integer to that buffer width.

Let H be SHA-256, `s` the original salt bytes, and `p` the UTF-8 password bytes:

1. Compute `h = H(p)`. PBKDF2 input is `h` for `s2k`, or lowercase ASCII `hex(h)` for `s2k_fo`.
2. Derive 32 bytes with PBKDF2-HMAC-SHA256, the server salt and iteration count.
3. Compute `x = H(s || H(":" || derived))`. The colon remains when the username is removed.
4. Compute `k = H(PAD(N) || PAD(g))`, `u = H(PAD(A) || PAD(B))` and
   `S = (B - k*g^x)^(a + u*x) mod N`.
5. Compute `K = H(minimal(S))`. M1 and M2 follow the pinned PySRP implementation, including
   the padded generator hash and minimal A/B encodings. A successful M2 transition exposes
   session decryption; a failed transition consumes and discards its state.
6. Verify the recovered negotiation HMAC over the advertised protocol string, selected
   protocol, encrypted session data and optional context. Decrypt CBC only after verification.
7. Parse authenticated plist fragments, retain the DSID, IDMS token, app key, continuation
   and additional unlock fields. The app-token checksum covers `apptokens`, DSID and
   `com.apple.gs.xcode.auth`. GCM authenticates `XYZ` as associated data and uses a 16-byte nonce.

The abbreviated recovered formula `H(s || H(p))` omits the colon. The original PySRP `gen_x`
retains it, and the constants recovered from the bundled `srp._pysrp` module also contain the
colon literal. Fixtures exercise that interpretation directly. UTF-8 bytes are preserved; the
library does not apply SASLprep or lowercase credentials. Account normalization belongs to
orchestration.

## Developer portal byte contract

The recovered GSA portal session sends the DSID as `X-Apple-I-Identity-Id`, the Xcode app token
as `X-Apple-GS-Token`, and `com.apple.gs.xcode.auth` as `X-Apple-App-Info`. Each call refreshes
anisette headers and mirrors `X-Apple-Locale` to `X-Apple-I-Locale`. A POST to
`/services/QH65B2/{action}.action?clientId=XABBG36SBA` carries an XML plist with `clientId`,
`protocolVersion`, a UUID request ID and `userLocale`. System actions add `teamId` and
`DTDK_Platform`, use the `ios/` path prefix even for tvOS, and add `subPlatform=tvOS` for tvOS.
The client reads `resultCode` before parsing action-specific fields and reports only the code
for service failures. It does not echo server response bodies or credentials.

Typed responses validate team, device, app-ID and certificate fields. The engine classifies a
team with `type=Company/Organization` as organization; otherwise one membership whose name
contains `free` classifies as free. Unknown team types are retained as `Other` rather than
silently treated as paid. A single team becomes the default; multiple teams remain unselected.
The engine's certificate and app-ID jobs use the in-memory GSA session. When team enumeration
failed during login, these jobs retry enumeration. A single team is selected directly; multiple
teams require a choice, which is retained for subsequent jobs. Certificate names are decoded
from DER subject CN when `certContent` is present; absent DER uses the serial as a display
fallback. `is_ours` remains false until signing-key persistence and public-key matching exist.
Explicit revocation first checks that the serial occurs in the selected team's certificate list.
No device or app-ID creation occurs automatically. A portal failure after GSA does not discard
the authenticated in-memory token, so account state and team state remain distinguishable.

## Limits, cancellation and memory

Remote responses are bounded to 64 KiB = 65,536 bytes after decompression. Each header name
has at most 256 bytes and each value at most 4096 bytes; at most 128 distinct headers are
accepted. Exact and ASCII-case duplicate names, unrelated transport headers, invalid header
bytes and missing required fields fail validation. The configured URL can use HTTP or HTTPS;
HTTPS uses certificate verification. Redirects are refused. Query parameters are preserved,
with exactly one `u` parameter containing the username hash, or an empty value for a check.
Connect timeout is 5 s; request timeout is 15 s. Failed responses do not populate the cache.

Cache reuse requires the same user, wall-clock age in [0 s, 30 s), monotonic age below 30 s,
the same 30 s bucket and a bucket offset below 27 s. The reconstructed comparison is uncertain
and contradictory; the implemented rule follows the explicit same-bucket reading of its recovered
constants: a 30 s window, a `% 30` bucket and a `< 27` guard. The clock is sampled after
acquiring the shared refresh lock. A cancelled request releases that lock and does not publish
a partial cache entry.

PBKDF2 iterations are bounded to [1, 1,000,000], salt to [1, 1024] bytes, password to 4096
UTF-8 bytes, and username to [1, 1024] bytes without NUL. The accepted server public value
satisfies `0 < B < N` and is at most 256 bytes. These are explicit resource/range checks,
not claims about every value a live Apple service might return.
Session reconstruction additionally requires nonempty, control-free username/DSID/token fields
bounded to 1024/128/16,384 UTF-8 bytes respectively. Reconstruction does not validate the
token with Apple; the next portal request establishes current service acceptance.

GSA, factor and portal responses are bounded to 1 MiB = 1,048,576 bytes after decompression.
Complete XML plists accept only Apple's canonical public DTD; decrypted plist fragments reject XML
declarations and DTDs. Both paths reject nesting deeper than 32 elements. GSA origin validation
requires HTTPS, except loopback HTTP for controlled fixtures. The GSA client keeps a private
cookie jar and refuses redirects. Five code submissions, three explicit SMS requests and one
post-factor login restart bound the second-factor state machine. Verification codes must contain
4–10 ASCII digits and match the server-advertised length. A complete mismatch switches to the
alternate anisette provider at most once; it does not loop on another mismatch.

Session ciphertext/context/token envelopes are separately bounded to 65,536 bytes.
CBC ciphertext must be nonempty and divisible by 16 bytes. GCM app keys of 16, 24 or 32 bytes
are accepted. Cryptographic comparisons use constant-time MAC/equality APIs;
integer exponentiation uses fixed-width RustCrypto Montgomery arithmetic. Required minimal
integer serialization still examines leading zero bytes, so whole-exchange constant-time
execution is unproven.

Session keys, ephemeral/private values, password-derived buffers, decrypted buffers and
retained token/header values have zeroizing owners. Authentication structs redact Debug;
network errors omit configured URLs and response bodies. This establishes behavior for those
owners and output paths. It does not establish erasure of every compiler/cryptographic-library
temporary or caller-owned copy. Parsing failures can leave allocator copies; complete memory
erasure and timing behavior require separate measurement/audit.

For group bit width n, exponent width e, PBKDF2 iteration count i and bounded input length m,
the conservative arithmetic bound is O(e*n² + i + m) time and O(n + m) space. Here n = 2048 bits;
the public-key, verifier and shared-secret exponents process 256, 256 and 512 bits, respectively.
The bound `a + u*x < 2^512` follows from `a,u,x <= 2^256 - 1`:
`(2^256 - 1)^2 + (2^256 - 1) = 2^512 - 2^256`.
Header validation costs O(m + h log h), with h <= 128. One provider shares one bounded cache
entry across clones. Network/decompression buffers and allocation overhead are additional.
PBKDF2/exponentiation are synchronous CPU work and must run outside the UI/async executor;
the authentication client uses blocking workers and joins them on cancellation before reporting
completion. Network and delegate waits are cancellation-aware. With a maximum of one provider
switch and one factor restart, the client performs at most three SRP exchanges; each successful
exchange uses three GSA requests. Factor requests add O(c + s) HTTP operations for code attempts
c ≤ 5 and SMS requests s ≤ 3. The 1 MiB response bound applies to each response, not their sum.
Portal record arrays contain at most 4096 elements; record identifiers must be unique within a
response. Portal parsing and uniqueness checks cost O(m + r log r) time for response bytes m and
records r ≤ 4096, with O(m + r) retained space before typed records are returned. Date fields
accept plist dates or ISO 8601/RFC 3339 strings and are normalized to UTC; the recovered
`never` expiration marker maps to no expiration. The portal client requires HTTPS except
literal loopback HTTP fixtures. Redirects are refused.

## Verification and provenance

Thirty-three `sl-apple` tests and the engine anisette/authentication tests cover independent vector
parity, M2/negotiation/GCM tampering, strict CBC padding, malformed schema/XML, input limits,
actual mock HTTP, decompression limits, cancellation, cache boundaries, concurrent refresh, query handling
and redaction. Ten authentication client tests additionally cover the GSA wire contract,
cookie jar, alternate provider, factor modes and bounds, cancellation, and proof rejection.
Six portal tests cover the QH65B2 envelope, authenticated header precedence, fixed action paths,
tvOS fields, typed responses, schema rejection, response limits and cancellation.
The engine tests exercise a real password prompt, remote anisette transport, GSA service-error
mapping, absent failed-account state, explicit refusal of password persistence, account-scoped
portal views, team choice, revocation serial checks and logout during a pending prompt. The CLI test
invokes the actual executable against the controlled HTTP server.

Eight checked-in vectors are generated with PySRP 1.0.22 and PyCryptodome 3.23.0 under
Python 3.14.7. They include `s2k`, `s2k_fo`, A=2, a short shared-secret encoding, leading-zero
salt, UTF-8 credentials, an empty password and AES-128/192/256 token keys. Both independent
SRP peers agree on K and authenticate each other's proof. The fixture source SHA-256 is
`a1600e7ee7b2b7b203f9cad488fd7e0a441297597743ba5433ed9c5193e7a359`.

```sh
curl --fail --location https://raw.githubusercontent.com/cocagne/pysrp/1.0.22/srp/_pysrp.py \
    -o target/auth-vectors/pysrp.py
python3 scripts/auth-vectors.py target/auth-vectors/pysrp.py target/auth-vectors/regenerated.json
cmp crates/sl-apple/tests/fixtures/grandslam.json target/auth-vectors/regenerated.json
cargo test -p sl-apple -p sl-engine -p sl-cli
```

Primary sources:

- [RFC 5054](https://www.rfc-editor.org/rfc/rfc5054.html), DOI 10.17487/RFC5054:
  group, padding and SRP arithmetic. GrandSlam's SHA-256/username treatment is a recovered variant.
- [PySRP 1.0.22](https://raw.githubusercontent.com/cocagne/pysrp/1.0.22/srp/_pysrp.py):
  original executable oracle for x, K, M1/M2 and serialization.
- [RFC 8018](https://www.rfc-editor.org/rfc/rfc8018.html), DOI 10.17487/RFC8018:
  PBKDF2 and CBC padding definitions.
- [NIST SP 800-38D](https://csrc.nist.gov/pubs/sp/800/38/d/final), DOI 10.6028/NIST.SP.800-38D:
  GCM authenticated decryption and non-96-bit IV processing.
- [RustCrypto crypto-bigint 0.5.5 exponentiation source](https://raw.githubusercontent.com/RustCrypto/crypto-bigint/v0.5.5/src/uint/modular/runtime_mod/runtime_pow.rs):
  fixed exponent-width modular operations used by this implementation.
- [AltSign authentication client](https://github.com/rileytestut/AltSign/blob/master/AltSign/Apple%20API/ALTAppleAPI%2BAuthentication.m):
  independent client evidence for the GSA operation sequence, proof check and trusted-device verification.
- [Fastlane Spaceship portal client](https://github.com/fastlane/fastlane/blob/master/spaceship/lib/spaceship/portal/portal_client.rb):
  independent client evidence for QH65B2 team and Xcode provisioning action names. Its newer
  portal host and request format are not assumed equivalent to the recovered client.
- Sideloadly 0.60's `isign.devapi` and `isign.anisette` modules, recovered by reverse
  engineering: the Apple client contract and uncertainty markers restated above.

## Assumption register

- A1: The recovered fork shares upstream x/M1/M2 encoding. Dependent result: recovered-client
  interoperability. Probe: pinned independent peers, minimal encodings and leading-zero salt;
  compare an authorized live GSA exchange.
- A2: The explicit same-bucket cache description resolves the uncertain reconstructed expression.
  Dependent result: cache fidelity. Probe: request counts at 26/27/30 s, reversed clocks, user
  changes and live OTP acceptance. Exact original cache control flow remains unproven.
- A3: Selected limits cover practical service responses. Dependent result: live interoperability.
  Probe: authorized captures of salt/iteration/body sizes; change explicit limits if evidence
  contradicts them. Live service sizes are unknown.
- A4: Independent fixture libraries implement the documented primitives. Dependent result:
  cryptographic interoperability for fixtures. Probe: pinned source hash, two peer agreement,
  deterministic regeneration and cross-library CBC/GCM decryption. Fixtures do not establish
  Apple endpoint acceptance or provisioned-device acceptance.
- A5: The recovered GSA field names, cookie use and response schema still describe the service.
  Dependent result: live sign-in interoperability. Probe: strict mock request/response fixtures,
  then authorized live trace comparison of each operation and status. Current availability is unknown.
- A6: An absent `idmsdata` selects GET trusted-device validation, while present `idmsdata`
  selects POST with a plist body; successful SMS submission has no `serviceErrors`. The recovered
  control flow is ambiguous at these branches. Dependent result: second-factor interoperability.
  Probe: both method/body fixtures, rejection fixtures and authorized live factor traces.
- A7: A complete-operation `-36607` indicates anisette mismatch, and a second factor requires
  one fresh login exchange after verification. Dependent result: provider fallback and session
  transition. Probe: mock retry/restart counts; compare authorized live status transitions.
- A8: A GSA token held in engine memory represents an authenticated account but does not prove
  portal access or survive process restart. Dependent result: the engine account summary and
  future provisioning. Probe: test login/logout/session lifetime against a controlled service;
  enumerate teams and validate token acceptance before enabling portal actions.
- A9: The recovered developerservices2 QH65B2 host, action paths, plist envelope and response
  keys remain accepted. Dependent result: live team enumeration and provisioning. Probe: mock
  byte-level requests and malformed responses; compare each operation with authorized live
  traces. A mock success does not establish current Apple service acceptance.
- A10: A 1 MiB response and 4096-record cap cover practical portal data; `expirationDate`
  uses plist date or RFC 3339 text. Dependent result: larger-account interoperability and quota
  calculations. Probe: authorized response sizes, date variants and records at the limits.
- A11: A developer certificate record's `certContent`, when present, is a DER X.509 certificate
  whose subject CN is suitable for display. Dependent result: certificate-list names. Probe:
  malformed DER and independently generated certificate fixtures; compare authorized portal
  records with their decoded subjects. Missing DER displays the serial without inferring a name.
- A12: A previously enumerated team list and selected default remain valid for subsequent portal
  jobs. Dependent result: account-scoped certificate, app-ID and revocation requests. Probe:
  prompt and team-ID mock fixtures; re-enumerate after a live membership change or portal error.
  Current membership freshness is unknown. Session snapshots prevent locks across HTTP waits;
  logout is checked before dispatch, but an already dispatched request can complete after logout.
- A13: A serial listed for the selected team still identifies the intended certificate when the
  subsequent revoke action reaches the service. Dependent result: explicit revocation. Probe:
  controlled list/revoke race fixtures and authorized live status comparison. The two requests
  are not atomic; a service rejection must be reported with its `resultCode`.

## Bounded observations and quality gates

- High impact: the missing colon or padded shared secret changes authentication outputs;
  independent vectors exercise both cases.
- High impact: the client now bounds 2FA restart and anisette fallback in mock exchanges;
  portal session expiry and durable account state remain unimplemented.
- High impact: an engine session is memory-only and cannot support unattended refresh;
  password-remember requests fail explicitly until keychain storage exists.
- High impact: a portal error after GSA leaves a session with no enumerated teams. The engine
  reports the portal failure separately and retries enumeration for account jobs; signing must
  require a selected, verified team.
- High impact: certificate revocation is an external mutation. The engine requires an explicit
  revocation call and a matching serial in the chosen team's current list; in-flight requests
  can complete if logout races with dispatch.
- Medium impact: certificate ownership is unknown without a retained private key. The current
  list deliberately reports `is_ours=false` for every record.
- Medium impact: QH65B2 may be unavailable or changed; fixed-action mock fixtures establish
  client encoding and parsing only, while live service compatibility is unknown.
- High impact: two recovered second-factor branches are ambiguous. Mock fixtures establish the
  chosen behavior, while current live parity remains unknown.
- Medium impact: cancellation and concurrent refresh can otherwise preserve stale OTP state;
  mock transport tests cover the implemented cache/lock behavior.
- Medium impact: bounded parsing and zeroizing owners do not establish complete timing/memory
  isolation. The limits and unproven properties are recorded above.
- Low impact: grouped layout must survive formatting across library, tests and generator script;
  review the formatted source under AGENTS.md.

QG1: this work requires protocol/implementation analysis only. QG2: assumptions and falsification
probes are registered. QG3: implemented claims map to tests; every remaining original requirement
stays open in the ledger. QG4: bit/byte/time bounds and exponent arithmetic are explicit. QG5:
source contradictions are identified and their selected interpretations are testable. QG6:
primary sources and independent executable provenance are recorded. QG7: bounded observations
include authentication state-machine and memory/timing implications. Full-goal completion remains
unproven and requires all remaining workflow evidence.
