# Evidence for the 2026-10-20 security review

Reviewed through commit: `62c841e758e648730bdf2e63b5a0eb062beca0a6`.
See the [report](../../security-review.md) for severity, prerequisites,
limitations and remediation.

| File | Contents |
|---|---|
| [validation.json](validation.json) | Commands, baseline counts, probe outcomes and untested scope |
| [cargo-audit.json](cargo-audit.json) | RustSec result and advisory DB revision; existing RSA ignore retained |
| [npm-audit.json](npm-audit.json) | Full dependency audit with the source-map-js advisory |
| [npm-production-audit.json](npm-production-audit.json) | Production audit: zero reported vulnerabilities |
| [probe-results.txt](probe-results.txt) | Selected output from the temporary Rust probes |
| [reproductions.patch](reproductions.patch) | Local Rust reproduction tests; not applied to the delivered source |
| [browser-path-probe.mjs](browser-path-probe.mjs) | Decoder and URL-resolution assertions; sends no requests |

The reproduction tests intentionally assert the behavior at the reviewed
commit. A successful fix should make the corresponding reproduction fail;
convert it into a regression that asserts the intended safe behavior.

## Reproduce the Rust findings

Use a clean disposable checkout of the reviewed commit with this evidence
directory available. The pilot tests use the existing `TestEnv` harness to
create/drop test databases; PostgreSQL must be available. The body probe uses
one local TCP connection and capacity one for 12 seconds. It is not a load test.

From the repository root:

```sh
git apply --check docs/evidence/security-review-2026-10-20/reproductions.patch
git apply docs/evidence/security-review-2026-10-20/reproductions.patch
cargo test -p trestle-core --test security_review_probe -- --nocapture
cargo test -p pilot --test accounts review_ -- --nocapture
cargo test -p trestle-identity review_stale_key_has_no_maximum_age -- --nocapture
git apply -R docs/evidence/security-review-2026-10-20/reproductions.patch
```

During the original run the three added pilot tests lived in a temporary copy
of `accounts.rs` named `security_review_probe.rs`. The preserved patch appends
only those tests to `accounts.rs`, reusing its identical harness. Eight copied
baseline account tests also passed in that run; their duplicate executions are
not added to the 395-test baseline count.

The JWKS probe sets cache timestamps directly to represent a failed refresh
followed by retry suppression. It verifies a real signature with the existing
test-only RSA fixture. It does not contact a provider or simulate a real
30-day outage. The database TLS probe exercises the production configuration
gate; certificate behavior was established by inspecting locked sqlx source.

## Reproduce browser path normalization

After the frontend package build (`npm test` in `frontend` builds packages):

```sh
node docs/evidence/security-review-2026-10-20/browser-path-probe.mjs
```

Expected output at the reviewed commit:

```json
{"path":"/api/%2e%2e/admin/","accepted":true,"resolved":"/admin/"}
{"path":"/api/%2E%2E/%2E%2E/metrics","accepted":true,"resolved":"/metrics"}
{"template":"/api/{id}/actions/run","id":"..","filled":"/api/../actions/run","resolved":"/actions/run"}
```

This uses the built client and Node's WHATWG URL implementation. It proves
decoder acceptance and destination normalization; it does not claim that a
particular target endpoint permits a privileged action.

## Audit reproducibility

Run `cargo audit --deny warnings --json` at the repository root, and
`npm audit --json` / `npm audit --omit=dev --json` in `frontend`.
These consult changing advisory databases, so future output may differ.
The captured JSON records the results used for this report.
