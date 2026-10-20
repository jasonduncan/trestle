# ADR-005: Browser identity and deployment origin

**Status:** Accepted for the pilot (M5, 2026-09-26). The identity provider is
not chosen yet; that choice is the project owner's. · **Design doc:** §12.1–§12.4,
§16.1, I06

## Context

Until M4 the only identity adapter was development bearer tokens, and
production refused to boot with it. The pilot needs a production browser
profile and a way for API clients to authenticate. The design doc asks for
an existing identity provider, not home-grown password or token
cryptography, and for server-side sessions. It also warns that CORS and
SameSite alone do not stop cross-site request forgery.

## Decision

**One public origin.** Product bundle at `/`, admin shell at `/admin/`,
`/api` and `/_meta` routed to the API replicas by the edge
(`deploy/Caddyfile`). No CORS is configured, so none is needed. Admin shares
the origin so it shares the host-only cookie and the login callback.

**Identity is an OpenID Connect integration** (`crates/trestle-identity`):

- **Discovery:** the discovered `issuer` must equal the configured one
  exactly. Provider URLs are `https` (loopback `http` only outside
  production), responses are size-limited, redirects are not followed, and
  requests time out.
- **Browser login** is the authorization code flow:
  - PKCE S256, `state` and `nonce`;
  - `client_secret_basic` for confidential clients.
  - The `state` rides in a `__Host-trestle_login` cookie (`SameSite=Lax`,
    10 minutes), so a callback completes only in the browser that started
    it.
  - Pending logins are single-use rows keyed by the state's digest.
  - `return_to` must be a same-site path.
- **ID tokens:** checked for signature, `iss`, `aud` = client ID, `exp`,
  `nbf`, `nonce`, and `azp` whenever there are several audiences or `azp`
  is present.
- **Signing keys:** asymmetric algorithms only (RS/PS/ES/EdDSA); `none`
  and HMAC are refused. The JWKS is cached for an hour, and an unknown
  `kid` triggers at most one refetch per 30 seconds. A successful fetch
  replaces the key set entirely (an empty set trusts nothing); if refetches
  fail, the old keys keep verifying for at most 24 hours after the last
  successful fetch, then tokens are refused as `identity_unavailable`.
- **Server-side sessions:**
  - The cookie is `__Host-trestle_session`: 256 random bits,
    `Secure; HttpOnly; SameSite=Strict; Path=/`, host-only.
  - The database stores only the SHA-256 of the token.
  - Absolute lifetime 12 h, idle timeout 2 h.
  - Revocation: logout revokes one session; `pilot identity revoke --actor`
    revokes all of an actor's sessions.
  - A new session is created on every login.
- **Forgery checks** apply to every cookie-authenticated request whose
  method is not GET, HEAD or OPTIONS, and cover all managed routes and
  logout:
  - `Origin` must exactly equal an allowed origin;
  - `X-CSRF-Token` must equal HMAC(session key, token digest).
  - The token comes from `GET /api/v1/auth/session`, is held in memory by
    the client, and is never stored.
  - Failure is 403 `csrf_failed`.
- **API clients** send `Authorization: Bearer <JWT>`:
  - Tokens are provider-issued JWT access tokens for
    `TRESTLE_OIDC_API_AUDIENCE`.
  - Boot refuses an API audience equal to the browser client ID, so an ID
    token can never be replayed as an API token.
  - Opaque access tokens (which would need introspection) are not
    supported.
- **The chain.** A request with `Authorization` never falls back to its
  cookie: one request, one credential.
- **Actors:** `ActorId` is UUIDv5 of issuer and subject (length-prefixed).
  It is stable across replicas and needs no lookup table. Memberships are
  provisioned with `pilot identity actor-id` and `pilot provision member`.

**Authorization is unchanged.** Identity yields only an actor. Tenant,
role and policy checks stay in the operation's context and policy (I06).

**Production boot refuses** development tokens, a missing provider, missing
or short keys, non-`https` provider or origin, an API audience equal to the
client ID, and a non-local database without `sslmode=require` or stronger
(`examples/pilot/src/server.rs`). All problems are reported at once.

## Consequences

- Changing identity providers changes actor IDs, so memberships must be
  remapped. This is a pilot limit, listed in the readiness document.
- A session lookup costs one indexed query per cookie-authenticated
  request. An idle-clock update happens at most once a minute per session.
- Unauthenticated login attempts write one row each (10-minute expiry, swept
  on the next login). Admission rate limits bound this per client address;
  a distributed flood is a residual risk for the edge to absorb.
- The OpenAPI document still describes bearer authentication only. Browser
  sessions are a transport concern of the first-party clients.

## Evidence

- `crates/trestle-identity` unit tests cover URL policy, cookie parsing,
  CSRF binding to session and key, the PKCE RFC 7636 vector, return-to
  validation and actor derivation.
- `examples/pilot/tests/identity.rs` runs against a mock provider on a real
  socket (test-only keys in `tests/fixtures/`). It covers:
  - the full login;
  - cookie attributes;
  - state replay and login CSRF;
  - nonce tampering;
  - provider refusal;
  - open redirects;
  - absolute and idle expiry, revocation;
  - digest-only storage.
  - Bearer tokens are tested with the wrong audience (an ID token), no
    audience, expired, not yet valid, wrong issuer, missing or empty
    subject, a forged signature, an unknown key, HMAC, `alg: none`, an
    opaque token, and a bearer token with a cookie.
- `examples/pilot/src/server.rs` tests every production boot refusal.
- `docs/evidence/m5-demo.json`, `m5-idp.png` and `m5-session.png` show a
  real Chrome run:
  - production boot refused;
  - OIDC login through a mock provider (PKCE, client authentication and
    redirect URI verified by the provider);
  - cookie attributes as specified;
  - a write carrying the CSRF token;
  - a same-site page on another origin whose write carried the session
    cookie and was refused `csrf_failed`;
  - sign-out, after which the replayed cookie gets 401.
- `docs/evidence/m5-db-tls.json` shows that `verify-full` to the database
  works and that plaintext connections, untrusted CAs and wrong host names
  are refused.

## Open

- Choose the identity provider, register the client, and run the login
  against it (the mock is not a substitute).
- External review of the session and CSRF design and of the login routes is
  required before the pilot (§12.4).

## Addendum (2026-09-26): password accounts

The project owner asked for Django-style username/password sign-in alongside, or
instead of, an identity provider. That is compatible with this ADR. The
design doc's reference profile uses a provider, and its prohibition is on
*home-grown cryptography*, not on passwords. Hashing uses the RustCrypto
`argon2` crate, as Django's uses a library.

**Decision:** `trestle_identity::password`, enabled with
`TRESTLE_PASSWORD_LOGIN`.

- **Hashing.** Argon2id with the OWASP parameters (19 MiB, 2 passes, 1
  lane), stored as PHC strings, hashed off the async runtime with bounded
  concurrency. Hashes made with weaker parameters are upgraded at the next
  sign-in.
- **Password rules** follow Django's validators:
  - at least 10 characters, at most 256;
  - not in the 8,356 entries of Django's common-password list that have 8
    or more characters;
  - not entirely numeric;
  - not built from the email address.
- **Operators create accounts.** `pilot account invite` issues a one-time
  link; `pilot account reset-link` issues a reset. The link secret:
  - is 256 random bits, stored as SHA-256;
  - expires (7 days for invites, 24 hours for resets);
  - works once, and a newer link voids older ones;
  - travels in the URL fragment, which the page removes from the address
    bar.
  - Setting a password ends every existing session.
  - There is no self-service "forgot password" yet: the server does not
    send email (ADR-011).
- **Sign-in.** `POST /api/v1/auth/password/login` requires an allowed
  `Origin` and a JSON body.
  - Every failure answers the same `login_failed` and costs one Argon2
    verification (a dummy hash when there is no usable account).
  - Five consecutive failures lock the account for 15 minutes.
  - Attempts are limited per client address (60 a minute by default,
    `TRESTLE_PASSWORD_ATTEMPTS_PER_MINUTE`; schools often share one
    address).
  - Success creates the same server-side session as OIDC sign-in, and any
    session this browser already held ends.
- **Change password** requires the session, CSRF and the current password.
  It ends every other session.
- **Actors.** A password account has its own actor ID, generated at
  invite. Memberships attach with `--school/--role`, or with
  `pilot provision member`.
- **Production.** Password sign-in alone satisfies the boot check. Session
  key and https public origin are still required.

**Evidence:**

- `crates/trestle-identity/src/password.rs` unit tests cover hashing,
  salting, rehash detection, the rules and email normalisation.
- `examples/pilot/tests/accounts.rs` covers:
  - the invite → set → sign-in flow and single-use links;
  - identical failures, lockout and its expiry;
  - the per-address limit;
  - Origin and JSON requirements;
  - link replacement and expiry, and sessions ending on reset;
  - change password;
  - disabled accounts;
  - the methods endpoint.
- `docs/evidence/accounts-demo.json` and three screenshots show the same
  flow in real Chrome, operated through the CLI with email sent to the
  file backend.

**Not included:** multi-factor authentication, breached-password (HIBP)
checks, self-service reset, and sign-up.
