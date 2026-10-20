# Security review (M5 self-review)

**This is not the external review the design doc requires (§12.4).** It is
the builders' own review. It maps each threat to a control and to the test
or demonstration that exercises it, lists dependency audit results, and
names what an external reviewer must examine.

Date: 2026-09-26. Commit: the M5 release commit (see `git log`).

## Threats and controls (design doc §12.3, extended for M5)

| Threat | Control | Proof |
|---|---|---|
| Mass assignment or hidden-field submission | Explicit input contracts; unknown fields rejected (`unknown_field`); ownership never in input | `tests/lessons_api.rs` (`rejects_ownership_fields`), `fixtures/contracts/lessons.create.input.json` (34 cases through JSON Schema, managed route and hand-written route) |
| Cross-tenant data or relation inference | Typed query surface requires a scope (`query::select(scope, value)`); membership from the database, never from the header alone; relation choices scoped; `schools.mine` scoped to the actor | `tests/lessons_api.rs`, `tests/resources.rs` (relation picker), `tests/additive.rs` (spoofed tenant header grants nothing), m4-demo |
| Stale permission or record state | `Policy::recheck` inside the transaction under `FOR SHARE`; ETag/`If-Match` on update and delete; 412 on stale | `tests/lessons_api.rs` (revocation between admit and commit), m5-demo step 4 |
| Metadata leakage or cache contamination | Descriptors require the operation's context; everything `Cache-Control: private, no-store` (perimeter default); published documents checked for ownership and policy leaks | `tests/meta.rs`, `tests/registry.rs` (`published_documents_do_not_leak_policy_or_ownership`), `tests/perimeter.rs` |
| UI schema XSS or remote code loading | Renderer decoder treats descriptors as untrusted: closed schema subset, reference keywords and prototype-polluting names rejected, text-only labels, local widget allowlist; edge CSP `script-src 'self'` | `frontend/interaction-react/test/protocol.test.ts`, `deploy/Caddyfile` (unvalidated) |
| Unsafe metadata URLs or external references | Bindings must stay under `/api/`; no external schema fetch; jsonschema test dependency built without network resolution | protocol tests; `Cargo.toml` (`jsonschema` without default features) |
| Expensive queries or schemas | Page cap 100; signed cursors bound to the query shape; body size limit; statement timeout (503 `database_timeout`); admission (503 `overloaded`) and per-credential token buckets (429 `rate_limited`) | `tests/perimeter.rs`, cursor tests in `trestle-core::pagination`, `docs/evidence/m5-load.json` (no errors under load) |
| Privileged route bypass | Route inventory: managed, unmanaged-with-waiver, system; generated commands only through the executor; admin profile owner-only | `pilot inspect routes`, `tests/registry.rs`, m4-demo step 4 |
| Supply chain or generated-code mistakes | Lockfiles; pinned majors; snapshots of every published document; append-only revision log; `#![forbid(unsafe_code)]` in every crate | `tests/published.rs`; the dependency audit below |
| **Session hijacking or fixation** (M5) | 256-bit random tokens; `__Host-` cookie with `Secure`, `HttpOnly` and `SameSite=Strict`; database stores only SHA-256; new session per login; absolute and idle expiry; revocation | `tests/identity.rs`, m5-demo step 6 (cookie attributes, replay after sign-out gets 401) |
| **Cross-site request forgery** (M5) | For cookie-authenticated unsafe methods: exact `Origin` allowlist and HMAC CSRF token; logout included | `tests/identity.rs` (five refusal variants), m5-demo step 6 (a same-site page on another origin, cookie sent, refused `csrf_failed`) |
| **Login CSRF, state replay, open redirect** (M5) | State bound to a `__Host-` login cookie; single-use pending logins; PKCE S256; nonce checked; `return_to` restricted to same-site paths | `tests/identity.rs` (`a_callback_completes_only_in_the_browser_that_started_it`, `tampered_or_refused_logins_create_no_session`) |
| **Token forgery or confusion** (M5) | Asymmetric algorithms only; issuer, audience, `exp`, `nbf` and `sub` required; API audience must differ from the client ID (boot check); JWKS refresh rate-limited | `tests/identity.rs` (`provider_access_tokens_authenticate_api_clients`: 12 refusal cases including `alg: none` and HMAC), `server.rs` tests |
| **Development identity in production** (M5) | Boot refuses development tokens and every other unsafe setting, all reported together | `server.rs` tests; m5-demo step 6 (`productionBootRefusal`) |
| **Database traffic interception** (M5) | rustls with aws-lc-rs; production requires `sslmode` of `require` or stronger for non-local hosts | `docs/evidence/m5-db-tls.json` (`verify-full` works; plaintext, untrusted CA and wrong host refused) |
| **Spoofed client address to dodge rate limits** (M5) | The client-address header is read only when configured, and the edge overwrites it (`header_up X-Real-IP {remote_host}`); credentialed requests are keyed by credential | `admission.rs`; `deploy/Caddyfile` (unvalidated) |
| **Operational data leakage** (M5) | Metrics labels bounded (no actor, tenant or record IDs, checked by test); metrics on a private listener; logs carry no bodies, tokens or cookies; provider error text sanitized | `tests/perimeter.rs` (`metrics_use_route_templates_and_operation_ids_never_record_ids`) |
| **Password guessing and credential stuffing** (accounts) | Argon2id (19 MiB, t=2); five-failure lockout for 15 min; per-address limit on every password endpoint; bounded concurrent hashing | `tests/accounts.rs` (lockout, per-address 429), `password.rs` unit tests |
| **Account enumeration** (accounts) | Identical `login_failed` for unknown, wrong, disabled, locked and no-password; each costs one Argon2 verification (dummy hash) | `tests/accounts.rs` (`sign_in_failures_look_alike_and_lock_the_account`) |
| **Leaked or replayed invite/reset links** (accounts) | 256-bit secrets stored as SHA-256, expiring, single-use, newest voids older; carried in the URL fragment (not sent to servers or in `Referer`) and removed from the address bar; setting a password ends every session | `tests/accounts.rs`, `test/auth.test.tsx`, accounts-demo step 1 |
| **Login CSRF and session fixation** (accounts) | Allowed `Origin` and a JSON body on every password endpoint; a new session per sign-in, ending any the browser held | `tests/accounts.rs` (`sign_in_needs_the_allowed_origin_and_a_json_object`) |
| **Email interception or downgrade** (email) | Production requires `smtps://` or STARTTLS-required `smtp://`; optional pinned CA; no plaintext credentials ever | `trestle-mail/tests/smtp.rs` (STARTTLS stripping, untrusted certificate) |
| **Email header injection** (email) | Recipients parsed as a single address; subject encoded by lettre | `trestle-mail` unit test `recipients_cannot_inject_headers` |
| **SQL injection** | All values bound as parameters; identifiers in generated migrations validated; typed query columns are constants | `trestle-historical::sql` tests (`refuses_unsafe_identifiers_from_artifacts`) |

## Dependency audit (2026-09-26)

| Tool | Scope | Result |
|---|---|---|
| `cargo audit --deny warnings` (cargo-audit 0.22.2, RustSec DB with 1,271 advisories) | 342 crates in `Cargo.lock` | Pass, with one documented ignore |
| `npm audit` and `npm audit --omit=dev` | `frontend/package-lock.json` | 0 vulnerabilities |

**The ignore is RUSTSEC-2023-0071** (the `rsa` Marvin timing side channel;
no fixed version exists). `rsa` is in the lockfile only as a dependency of
`sqlx-mysql`, which sqlx declares optionally. This workspace enables only
the PostgreSQL driver, and `cargo tree -i rsa --target all` confirms the
crate is never built. JWT verification uses aws-lc-rs through
`jsonwebtoken`'s `aws_lc_rs` backend. The ignore and its reason live in
`.cargo/audit.toml`; re-check it whenever sqlx features change.

**Notes on the supply chain:**

- TLS and JWT cryptography come from aws-lc-rs (C and assembly). That is
  vendor code outside `forbid(unsafe_code)`; the choice avoids `ring` and
  the pure-Rust `rsa` crate.
- The dependency graph is larger in M5 because of reqwest, rustls and
  aws-lc. The majors are pinned and the lockfiles committed.
- No code generation runs at request time. Generated artifacts (migrations,
  SDK, published documents) are committed and reviewed as diffs.

## Residual risks (accepted for the pilot, pending review)

1. **Login-row flooding.** Each unauthenticated `GET /api/v1/auth/login`
   writes a row (10-minute expiry). Per-address rate limits bound a single
   source; a distributed flood must be absorbed at the edge.
2. **Rate limits are per replica and in memory.** N replicas allow up to N
   times the configured rate. They are not a substitute for edge limits.
3. **Actor IDs derive from issuer and subject.** A provider migration
   changes every actor ID; memberships must be remapped with a script.
4. **No refresh of provider-side state.** A person disabled at the provider
   keeps a live session until expiry (at most 12 h, or 2 h idle) unless an
   operator runs `pilot identity revoke`. Back-channel logout is not
   implemented.
5. **The edge configuration is unvalidated.** `deploy/Caddyfile` has not
   been loaded by Caddy. The same routing was exercised only by the Node
   edge in the M5 demonstration.
6. **The CHECK-on-add migration takes an exclusive lock** while validating.
   That is acceptable at pilot sizes; see `docs/runbooks/deploy.md`.
7. **Self-review only.** Everything above is the builders' own reading.
8. **Password accounts: no multi-factor authentication** and no
   breached-password (HIBP) check. The common-password list is Django's.
9. **Lockout denial of service.** Anyone who knows an address can lock that
   account for 15 minutes at a time. Operators can clear it with
   `pilot account enable`.
10. **Links are bearer secrets in email.** Whoever reads the message first
    can set the password. Links are short-lived and single-use, and
    setting a password ends every other session.

## External review required before the pilot (§12.4)

| Area | Where |
|---|---|
| Policy boundaries and tenant scoping | `examples/pilot/src/{schools,lessons,units}/`, `crates/trestle-core/src/{operation.rs,query.rs}` |
| Session, CSRF and login flow | `crates/trestle-identity/src/{session.rs,routes.rs,oidc.rs,chain.rs}` |
| Password accounts and links | `crates/trestle-identity/src/password.rs`, `frontend/interaction-react/src/{auth.tsx,session.ts}` |
| Outgoing email | `crates/trestle-mail/src/lib.rs`, `examples/pilot/src/mail.rs` |
| Metadata resolver and renderer decoder | `crates/trestle-core/src/registry/meta.rs`, `frontend/interaction-react/src/protocol.ts` |
| State-changing operations and the executor | `crates/trestle-core/src/{operation.rs,db.rs,registry/routes.rs}`, `examples/pilot/src/lessons/operations.rs`, `examples/pilot/src/provision.rs` |
| Perimeter and deployment | `crates/trestle-core/src/{admission.rs,http.rs}`, `examples/pilot/src/server.rs`, `deploy/` |
