# Security review — 2026-10-20

Reviewed through commit: `62c841e758e648730bdf2e63b5a0eb062beca0a6`
(0.6.0 plus the npm namespace change).
This is a source review, dependency audit and local adversarial test pass by
Codex. It did not itself close the external-review gate in design §12.4; the
project owner did (below).
The current AGENTS.md records owner pilot approval on 2026-10-20; this review
does not grant or replace that decision. The review itself changed only
documentation and evidence.

**Remediation (2026-10-20):** SR-01 to SR-06 are fixed and SR-07 is mitigated
in 0.7.0; each finding's **Status** line names the change and its test. The
findings below are kept as written at review time.

**§12.4 gate (2026-10-21):** closed by the project owner, who accepted this
review, with its findings fixed in 0.7.x (0.7.1 on crates.io), as the external
review §12.4 requires. It covers the four areas §12.4 names: policy boundaries
(identity → tenant policy), session and CSRF behaviour (browser → session),
the metadata resolver (metadata → UI) and state-changing operations
(transport → executor, retry/worker → writes); see the scope table below.
The owner plans a more comprehensive review later. Review again before
distributing restricted API keys (SR-07) or after changing any of those
boundaries.

The most urgent finding is an unauthenticated availability attack: stalled
JSON uploads can retain all admission permits. Other findings concern password
recovery, production database TLS, OIDC key retirement, browser URL validation
and a development dependency. API-key scope enforcement is a separate,
conditional integration risk.

The [2026-09-26 M5 self-review](security-review-2026-09-26.md) is preserved
unchanged as history. Its clean npm audit and several security assurances must
not be read as current results. In particular, `/api/` confinement has a bypass,
the database TLS drill covered `verify-full` rather than every accepted
production mode, and JWT `nbf` is checked **when present**, not required.

## Findings at a glance

Severity reflects the exposure established here, with prerequisites below;
it is not an upstream CVSS score or a claim about an inspected deployment.

| ID | Severity | Finding | Exposure and evidence |
|---|---|---|---|
| SR-01 | High | Incomplete JSON bodies retain global admission capacity without a deadline | Public password endpoints; real local TCP reproduction |
| SR-02 | Medium | Password change leaves existing reset links usable | Requires possession of an unused, unexpired link; real pilot/DB reproduction |
| SR-03 | Medium | Production accepts database TLS without full server identity verification | Requires weaker accepted configuration and an on-path attacker; boot-gate reproduction and driver inspection |
| SR-04 | Medium | OIDC accepts cached signing keys without a maximum stale age | Optional OIDC; retired private key compromise plus refresh failure; deterministic verifier reproduction |
| SR-05 | Medium | Browser binding validation permits paths outside `/api/` | Untrusted descriptor or string path parameter; decoder and URL-resolution reproduction |
| SR-06 | Low for this project | Vulnerable development dependency and no npm audit CI gate | Upstream High advisory; no production request path established |
| SR-07 | Medium, conditional integration risk | API-key scope labels do not constrain the built-in identity adapter | Optional adapter wired explicitly in probe; not configured by stock pilot startup |

### SR-01 — Stalled uploads can exhaust admission permits

**Status:** Fixed in 0.7.0 (#40, `6b6064d`). Request bodies have a total deadline (`TRESTLE_BODY_TIMEOUT_MS`, 10 s; 408 and the permit returns), also in the sign-in audit middleware. The Caddyfile gained `read_header`/`read_body`/`idle` timeouts and a 1 MB body cap. Verified over real TCP (`trestle-core/tests/body_timeout.rs`) and through a real Caddy (`make drill-edge`, [evidence](evidence/edge-drill.json)).

**Location:** [input.rs](../crates/trestle-core/src/input.rs), lines 29–55;
[admission.rs](../crates/trestle-core/src/admission.rs), lines 216–247;
[password.rs](../crates/trestle-identity/src/password.rs), lines 819–826;
[Caddyfile](../deploy/Caddyfile).

`JsonBody` limits bytes but awaits the complete body without a time limit.
Admission acquires a global semaphore permit before invoking the handler and
holds it through that wait. Password login is public; its body extractor runs
before the handler's origin check and password-specific throttle. A client
does not need credentials or an allowed origin to occupy this path.

**Observed:** a local TCP client declared a 100-byte JSON body, sent only `{`,
and kept the connection open. With admission capacity set to one, an unrelated
request still received `503 overloaded` after 12 seconds. The probe did not
stress the default capacity of 256; exhausting that capacity follows from the
same permit lifetime and absence of a body deadline. Address rate limits slow
arrival but do not reclaim permits held by unfinished uploads.

The supplied Caddy configuration adds no upload deadline. Caddy documents that
`read_body` has no timeout by default; the edge therefore cannot be assumed to
close this gap. [Caddy timeout documentation](https://caddyserver.com/docs/caddyfile/options#timeouts).

**Fix:** bound body acquisition time before operation execution, release permits
on timeout/disconnect, and configure edge upload time/idle limits appropriate to
the API. Avoid treating a blanket timeout around writes as proof that a commit
did not happen; preserve the `outcome_unknown` contract.

**Acceptance:** a stalled unauthenticated password request expires within the
configured limit; unrelated traffic resumes; slow trickles cannot extend a
total deadline indefinitely. Cover direct listener and actual edge behavior.

### SR-02 — An old reset link survives an authenticated password change

**Status:** Fixed in 0.7.0 (#37, `1675b28`). A password change deletes unused invite/reset links in its transaction; issue, change, disable and redemption all lock the account row before links. Regression and race tests in `examples/pilot/tests/accounts.rs`.

**Location:** [password.rs](../crates/trestle-identity/src/password.rs),
lines 919–950 and 1032–1047.

Authenticated password change updates the hash and revokes other sessions, but
does not invalidate unused account links. `set_from_link` checks expiry,
single use and account status, without checking whether the password changed
after link issuance.

**Observed:** invite and activate an account; issue a reset link; change the
password with the correct current password, session and CSRF token (`204`);
then submit the earlier link with another password (`200`). Signing in with
that replacement password also returned `200`. All operations used the real
pilot handlers and a disposable PostgreSQL database.

An attacker already holding that link retains a recovery credential after
the owner changes their password. This is not a token-guessing attack and does
not let someone without a link reset an arbitrary account.

**Fix:** invalidate outstanding invite/reset credentials in the same transaction
as password change, or bind them to a credential generation checked atomically.
Use consistent account/token lock ordering across issue, change and redemption
so concurrent recovery cannot revive a credential invalidated by the change.

**Acceptance:** an unused reset link issued before a successful change becomes
`token_invalid`; a subsequently issued link works; racing change/redemption
preserves a documented ordering and session-revocation guarantee.

### SR-03 — Production TLS checks allow unauthenticated database encryption

**Status:** Fixed in 0.7.0 (#39, `e95be19`). Production requires `sslmode=verify-full` for a non-local database, and the boot checks now run before any connection. `make drill-tls` records that `require` accepts an untrusted certificate. With sqlx-core 0.8.6 and rustls 0.23, `verify-ca` did refuse a mismatched host in the drill, so that part of the finding depends on the driver version; only `verify-full` is accepted either way.

**Location:** [serve.rs](../crates/trestle/src/serve.rs), lines 295–305;
[production environment example](../deploy/pilot.env.example), line 8.

For a non-local database, production boot accepts `require`, `verify-ca` or
`verify-full`. Inspection of the locked `sqlx-postgres 0.8.6` implementation
(`src/connection/tls.rs`) shows that `require` accepts invalid certificates and
hostnames; `verify-ca` checks the certificate chain but accepts a mismatched
hostname. Encryption alone does not authenticate the intended database server.
PostgreSQL recommends `verify-full` for security-sensitive use.
[PostgreSQL TLS documentation](https://www.postgresql.org/docs/current/libpq-ssl.html).

**Observed:** otherwise valid production settings with
`postgres://pilot@db.internal/pilot?sslmode=require`, and then `verify-ca`,
both produced an empty `ServeConfig::problems` result. This verifies the boot
gap, not a live man-in-the-middle attack.

An on-path attacker can impersonate the database under `require`; under
`verify-ca`, an attacker also needs a certificate trusted by the configured CA.
The supplied environment example already uses `verify-full`, so this finding
concerns a weaker configuration that the production guard explicitly accepts.
The old `verify-full` drill does not establish safety for those weaker modes.

**Fix:** require `verify-full` for non-local production databases, or introduce
a narrowly documented exception with an equivalent authenticated transport.

**Acceptance:** production rejects `require`/`verify-ca` by default; real
connection tests reject untrusted certificates and the wrong hostname under
every supported production configuration.

### SR-04 — Failed OIDC refresh leaves old signing keys trusted indefinitely

**Status:** Fixed in 0.7.0 (#38, `14284ac`). Cached keys are trusted for at most 24 h after the last successful fetch, then verification is unavailable; a successful fetch replaces the key set, including an empty one. Refresh outcomes are counted (`trestle_oidc_jwks_refresh_total`, `trestle_oidc_jwks_expired_total`). Mock-IdP tests cover 503, malformed, timeout, empty and recovery.

**Location:** [oidc.rs](../crates/trestle-identity/src/oidc.rs),
lines 35–39 and 389–440.

The one-hour JWKS TTL triggers refresh but is not a maximum trust lifetime.
When refresh fails, `find` selects a key from the previous cache without
checking its age. Repeated failures can extend trust indefinitely. A successful
fetch with no usable keys also leaves the previous key set in place.

**Observed:** a deterministic unit probe populated the cache with a real test
RSA public key fetched 30 days earlier and a recent refresh-attempt timestamp,
matching the state immediately after a failed refresh. `Provider::verify`
accepted a freshly signed token for the configured issuer and audience. The
probe exercised signature/claims verification; it simulated the cache state
and did not cause or test a real provider outage.

If a retired private signing key is compromised and refresh keeps failing,
an attacker can mint fresh tokens accepted by a still-running instance.
Ordinary JWT expiry does not bound this attack because the attacker chooses
a new expiry. A successful nonempty refresh that removes the key stops it.

**Fix:** define a bounded stale-key grace period and fail authentication as
unavailable after it. Handle an authoritative empty/retired key set explicitly,
and expose bounded refresh-health metrics without logging tokens.

**Acceptance:** cached keys work only within the intended outage grace;
expired cache trust refuses fresh tokens; successful retirement removes trust.
Exercise timeout, malformed response, empty JWKS and recovery with a mock IdP.

### SR-05 — URL normalization bypasses browser API-prefix checks

**Status:** Fixed in 0.7.0 (#35, `c096790`). Descriptors reject encoded dot segments and look-alike prefixes; path parameters `.`/`..` are refused; the final URL is checked for origin and segment-boundary prefix before fetch (metadata and operations separately); requests use `redirect: "error"`. A refused request dispatches nothing (fetch-spy tests).

**Location:** [protocol.ts](../frontend/client/src/protocol.ts), lines 361–370;
[transport.ts](../frontend/client/src/transport.ts), lines 83–84 and 99–135.

`checkBindingPath` checks the raw prefix and literal `..`. Encoded dot segments
pass. URL resolution subsequently shortens the path. Separately, `fillPath`
uses `encodeURIComponent`, which leaves a parameter equal to `..` intact,
and the transport does not revalidate the final path.

**Observed with the built client and an otherwise valid descriptor:**

| Input | Decoder/result | Resolved pathname |
|---|---|---|
| `/api/%2e%2e/admin/` | Descriptor accepted | `/admin/` |
| `/api/%2E%2E/%2E%2E/metrics` | Descriptor accepted | `/metrics` |
| `/api/{id}/actions/run`, `id = ".."` | `fillPath` yields `/api/../actions/run` | `/actions/run` |

This behavior follows the URL standard's encoded double-dot handling.
[WHATWG URL path-segment definitions](https://url.spec.whatwg.org/#double-dot-url-path-segment).

A malicious descriptor or attacker-influenced string path parameter can direct
an authenticated request outside the claimed API prefix on the same origin.
No cross-origin escape, backend authorization bypass or working privileged
target was demonstrated. Pilot UUID parameters further limit the parameter
variant; the untrusted-descriptor boundary still fails.

**Fix:** validate the canonical destination after parameter substitution,
including origin and path-segment prefix boundaries. Reject encoded/literal
dot segments, test mixed case and repeated traversal, and explicitly decide
whether redirects may leave the permitted destination. Keep trusted metadata
fetches distinct from operation bindings when applying the check.

**Acceptance:** the payloads above are refused before fetch; normal encoded
parameters work; a fetch spy verifies that rejected inputs dispatch no request.

### SR-06 — Development dependency has a known denial-of-service advisory

**Status:** Fixed in 0.7.0 (#35, `c096790`). `source-map-js` 1.2.2; CI fails on any production advisory and on high/critical advisories in any dependency.

**Location:** [frontend/package-lock.json](../frontend/package-lock.json),
lines 2015–2023; [CI workflow](../.github/workflows/ci.yml), lines 113–120.

The lockfile selects `source-map-js 1.2.1` with `dev: true`. `npm audit`
reported GHSA-68fv-2mgg-jv7q / CVE-2026-93749, an upstream High advisory for
event-loop denial of service while processing malicious indexed source maps.
The fixed version is `1.2.2`.
[Advisory](https://github.com/advisories/GHSA-68fv-2mgg-jv7q),
[maintainer's release](https://github.com/7rulnik/source-map-js/releases/tag/v1.2.2).

**Observed:** the full npm audit reports one High vulnerability; the production
audit reports zero. No remotely supplied source-map path in the deployed
application was identified, so this review rates the established project
exposure Low: build/development availability when processing untrusted maps.

CI installs, tests and builds the frontend but has no explicit npm audit gate.
`npm ci` can print advisories and still succeed.

**Fix:** update the lockfile to a compatible fixed release and add an explicit
audit gate, with any exceptions documented and time-bounded.

**Acceptance:** full and production audits pass; package builds, tests and
typechecks remain green. This review did not update the dependency.

### SR-07 — API-key scopes need an explicit enforcement integration

**Status:** Mitigated in 0.7.0 (#36, `1da9145`). `ApiKeyIdentity::new` refuses keys with scope labels (as an invalid key) unless `accept_unenforced_scopes()` is set; docs state that a key carries its actor's full authority. An enforced restricted-key mechanism is not built; use a service actor whose roles limit it.

**Classification:** conditional integration risk, not a demonstrated stock-pilot
authorization bypass.

**Location:** [api_keys.rs](../crates/trestle-identity/src/api_keys.rs),
lines 8–14 and 278–286; [serve.rs](../crates/trestle/src/serve.rs),
lines 317–382; [README](../README.md), line 152.

The module correctly documents scopes as opaque strings interpreted by the
project. However, the supplied `ApiKeyIdentity` converts `KeyInfo` to only its
`ActorId`, discarding key identity and scopes. Ordinary actor-based policies
therefore see the actor's authority without a per-key restriction. The README's
description of “scoped API keys” can lead integrators to assume otherwise.

**Observed:** in a disposable pilot database, explicitly install the optional
API-key model and adapter, issue a teacher's key with the label `lessons.read`,
and POST a lesson. Creation succeeds (`201`). This label has no framework-defined
meaning; the probe demonstrates that stored labels alone enforce nothing.
Stock `ServeConfig::identity` does not install this adapter.

**Fix:** document the actor-wide authority of the adapter prominently. Before
offering restricted keys, provide a verified restriction mechanism checked at
execution, or use service actors whose ordinary policies actually limit access.
Consider failing closed for nonempty scope labels when no enforcement is
configured. Keep tenant/role authorization in operations and preserve the
identity adapter's actor-only contract.

**Acceptance:** a documented restricted-key integration cannot invoke a write
outside its allowed operations, through HTTP or any supported invocation path.
Revocation and membership rechecks must still apply.

## Scope, trust boundaries and controls examined

The review started with the M5 report, README, AGENTS instructions and design
§§2, 6.2, 7.2, 12, 14 and 18, then traced the current implementation. The
threat model includes anonymous clients, authenticated users of another tenant,
users with stale or weaker roles, malicious browser metadata and captured
credentials. Operator-authored Rust, configuration, intent files and migration
artifacts are trusted code/configuration; they are not a sandbox for hostile
application authors.

| Boundary | Review and exercised controls | Evidence in the existing suite |
|---|---|---|
| Request → contract | Explicit writable fields, unknown-field refusal, scalar parity, JSON size limits and safe problem bodies | `examples/pilot/tests/{conformance,lessons_api,perimeter}.rs`; protocol/client validator tests |
| Identity → tenant policy | Tenant membership read from DB; tenant-scoped SQL; cross-tenant rows and references concealed; write recheck under membership lock | Pilot `lessons_api`, `resources`, `additive`; `crates/trestle/tests/{intent_permission,intent_relations}.rs` |
| Public transport → executor | Managed routes and trusted invocations share execution; typed/dynamic outputs validated before commit; failed validation rolls back | `crates/trestle-core/tests/executor.rs`; pilot `invocation.rs`; intent parity tests |
| Retry/worker → writes | Receipt claimed after authorization/recheck; replay remains authorized; jobs enqueue transactionally and reauthorize; bounded attempts/leases | `crates/trestle-core/tests/{idempotency,jobs,effects}.rs` |
| Running build → schema | Fence precedes write transaction; old writers refused; immutable migration digest checks; drift refusal, bounded impact queries and transactional checkpoints | Core `fence.rs`; historical `runner`, `fence`, `impact`, `batched`, `non_atomic` tests |
| Browser → session/account | Hashed random session/link secrets, host-only secure cookies, exact origin and CSRF checks, single-use links, dummy verification and lockout; authorization-chain cookie fallback refusal | `examples/pilot/tests/{accounts,identity}.rs`; session/auth frontend tests; SR-02 remains |
| IdP/mail → application | Asymmetric JWT algorithms, issuer/audience/expiry verification, PKCE/state/nonce; bounded provider responses; mandatory production SMTP TLS and header parsing | Pilot mock IdP tests; `crates/trestle-mail/tests/smtp.rs`; SR-04 remains |
| Metadata → UI | Closed decoder subset, no schema reference loading, prototype-pollution guards, local widgets, text rendering; admin operations retain server authorization | Frontend protocol/views/operation tests; `crates/trestle/tests/{intent_admin,views}.rs`; SR-05 remains |
| Public edge → service | Admission, rate limits, statement timeouts, no-store responses, bounded metric labels and production boot checks | Pilot `perimeter.rs`; server tests; SR-01/SR-03 and live-edge qualification remain |
| Source/dependencies → artifact | Lockfiles, published snapshots, offline exports, model-history check, dependency review and limited tracked-file secret-pattern scan | Pilot `published`, `offline`, `registry`, `schema`; audits below |

These are inspected and exercised controls, not a proof that every branch is
safe. No additional confirmed tenant-isolation, SQL-injection, mass-assignment,
CSRF, output-contract, job-replay or migration-integrity bypass was found.
An adversarial metrics hypothesis was disproved: 32 custom HTTP methods on a
registered route returned 405 and created zero attacker-named request counters.

## Validation and dependency audit

All results below were obtained during this review against the reviewed
source. Temporary probes were removed from normal test/source paths and
preserved as an optional patch in the [evidence bundle](evidence/security-review-2026-10-20/README.md).

The shared checkout advanced during the review from
`756c197f7ff5922c94f275ab3585c2ada5db8e12` to the commit above. The intervening
diff was reviewed: it renames frontend packages/imports, records owner approval
and makes OIDC optional in the environment example. Security-critical Rust and
client validation/transport implementations did not change. Third-party npm
versions, resolved URLs, integrity hashes and development flags are identical.
The frontend test output already uses the renamed packages.

| Check | Result |
|---|---|
| `cargo test --workspace --locked`, Rust 1.98.1 | **395 passed, 0 failed, 1 ignored**; PostgreSQL integration tests included |
| `npm test` after `npm ci --ignore-scripts` | **175 passed**, 15 test files; includes package build |
| `npm run typecheck` after package build | Passed across frontend workspaces |
| `pilot check` | Passed; 10 managed, 10 unmanaged and 8 system routes |
| `pilot makemigrations --check` | No changes; models match committed history |
| Targeted Rust probes | Six review checks passed while asserting the observed behavior, including the negative metrics check |
| Client URL probe | Encoded paths and substituted dot segment reproduced; no network requests sent |
| `cargo audit --deny warnings --json`, cargo-audit 0.22.2 | **0 unignored vulnerabilities/warnings**; 385 dependencies, 1,345 advisories |
| `cargo tree -i rsa --target all --locked` | Nothing to print; existing RSA exception still outside enabled graph |
| `npm audit --json` | **1 High development-dependency advisory**, SR-06 |
| `npm audit --omit=dev --json` | **0 vulnerabilities** |

RustSec database commit:
`b19b2c83c2493961ccb12f9fc4699a00fb2c6c44`, updated
`2026-10-20T13:02:37+02:00`. The existing
[RUSTSEC-2023-0071 exception](../.cargo/audit.toml) covers `rsa` through
optional `sqlx-mysql` lockfile entries. It remains justified by the enabled
dependency graph and must be rechecked when features change. Cryptography
includes aws-lc C/assembly dependencies outside the workspace's Rust
`forbid(unsafe_code)` restriction.

The secret-pattern scan of tracked files identified only the documented
[test IdP RSA keys](../examples/pilot/tests/fixtures/README.md); it was not
a full entropy-based or git-history scan. No claim is made about external
secret stores or deployed environment values.

Initial sandbox restrictions prevented PostgreSQL/network access; the relevant
commands were rerun with access and completed. An initial frontend typecheck
before package outputs existed failed resolution; the ordered rerun passed.
Neither setup failure is reported as an application defect.

## Hardening observations and remaining qualification

These items are not additional confirmed exploit findings:

- **CSV exports:** `examples/pilot/src/lessons/export.rs:69` guards a leading
  `=`, `+`, `-` or `@`, but does not normalize leading control characters.
  Validate harmless formula-like cells with tab/CR/LF prefixes in the supported
  spreadsheet clients; no spreadsheet execution was tested here. Import
  behavior differs by client. [OWASP CSV injection guidance](https://community.owasp.org/attacks/CSV_Injection).
- **Browser response limits:** `transport.ts` reads response text and parses
  JSON before the decoder enforces its descriptor byte limit. Consider a
  streaming cap to bound memory/parse work from oversized metadata responses.
  No hostile-server memory exhaustion was measured.
- **Credential precedence:** `Chain` correctly refuses Authorization-to-cookie
  fallback. Session-management handlers using `Sessions` directly deserve
  explicit mixed-credential tests against the repository-wide rule. Existing
  origin/CSRF checks still apply; no CSRF bypass was established.
- **Supply chain:** CI action major tags remain mutable. Consider commit
  pinning and automated reviewed updates. No compromised action was identified.

Previously documented operational risks still need an owner decision:
in-memory rate limits multiply across replicas; distributed login-state row
creation needs edge controls/retention; password lockout permits targeted
denial of service; no MFA or breached-password service is integrated; emailed
links are bearer secrets; provider account disabling does not automatically
revoke already issued local credentials. Account/credential revocation must
be explicit, including optional API keys.

This pass did not run the opt-in Playwright views branch, the full browser
demo/compatibility suite, restore/load/TLS drills, or a live Caddy deployment.
It did not qualify a real SMTP relay or OIDC provider, inspect production
network isolation/DB grants/backups, fuzz all parsers, or verify vendor crypto.
Mock integration tests and old drill artifacts do not substitute for those
checks.

**Recommended remediation order:** close SR-01 before internet exposure;
fix SR-02 and require authenticated database TLS before using real accounts
and data; bound OIDC stale-key trust before enabling that provider path;
repair client URL validation and update the development dependency; resolve
scope enforcement before distributing restricted API keys. Then rerun focused
regressions and the relevant deployment/browser checks. Closure of the
external-review requirement was a separate decision from the owner's pilot
approval; the owner made it on 2026-10-21 (see the top of this document).
