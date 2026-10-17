# Runbook: operating the pilot

## Health

- `GET /healthz` is liveness (the process answers). `GET /readyz` is
  readiness: the database is reachable and every required migration is
  applied.
- Both are exempt from admission control and rate limits. The edge does not
  expose them publicly; probe replicas directly.

## Metrics (`TRESTLE_METRICS_BIND`, private network only)

Labels are bounded: route templates, methods, status classes, operation
IDs and problem codes. Actor, tenant and record IDs are never labels.

| Metric | Watch for |
|---|---|
| `trestle_http_requests_total{method,route,status}` | 5xx share above 0.5% for 5 min |
| `trestle_http_request_duration_seconds{method,route}` | p95 above 250 ms on list/retrieve for 10 min |
| `trestle_operation_total{operation,outcome}` | `internal`, `outcome_unknown`, `database_timeout`, `database_unavailable`; `schema_fenced` outside a rollout (an old replica is still running) |
| `trestle_operation_duration_seconds{operation}` | regressions after a deploy |
| `trestle_db_acquire_seconds` | p95 above 50 ms: pool too small or a slow query holds connections |
| `trestle_db_pool_connections{state}` | open equals pool size with idle at 0 persistently |
| `trestle_http_in_flight` | close to `TRESTLE_MAX_IN_FLIGHT` |
| `trestle_admission_rejected_total{reason}` | `overloaded` (capacity) or `rate_limited` (abuse or a misbehaving client) |
| `trestle_interaction_stale_total{operation}` | non-zero outside a rollout: an old revision was not retained |
| `trestle_auth_failures_total{reason}` | `csrf` (attack traffic, or `TRESTLE_PUBLIC_ORIGIN` not matching the browser origin); `unavailable` (provider or session store); a jump in `invalid` (credential stuffing or an expired client secret) |

## Logs

- One `request completed` event per request, with `request_id`, method,
  path, status and elapsed milliseconds.
- Problem responses carry the same `request_id`, so a user can quote it.
- Bodies, tokens, cookies and CSRF tokens are never logged.
- Provider error codes are logged sanitized (printable ASCII, 100
  characters).

## People and tenants

```sh
# The actor ID for a provider account (offline).
pilot identity actor-id --issuer https://id.example.com --subject <sub>
# Create a school, then grant a role. Both are audited as the operator actor.
pilot provision school --name "Northside Elementary"
pilot provision member --school <school-id> --actor <actor-id> --role owner|teacher|viewer
pilot provision remove-member --school <school-id> --actor <actor-id>
# End every browser session of a person, e.g. after offboarding.
pilot identity revoke --actor <actor-id>
```

Removing a membership takes effect on the member's next request. Writes
recheck membership inside their transaction.

## Password accounts and email

```sh
# Invite: creates the account (and a membership), emails a one-time link.
pilot account invite --email teacher@school.org --school <school-id> --role teacher --send
# Without --send the link is printed. Pass it on privately; it works once.
pilot account invite --email teacher@school.org
# Reset: earlier links stop working; setting the password ends every session.
pilot account reset-link --email teacher@school.org --send
pilot account list                       # invited | active | locked | disabled
pilot account disable --email x@y.org    # ends sessions, voids links
pilot account enable --email x@y.org     # also clears a lockout
pilot mail test --to ops@school.org      # proves the email settings end to end
```

- **Links** use the first `TRESTLE_PUBLIC_ORIGIN`: `…/set-password#token=…`.
  Invites last 7 days and resets 24 hours; `--valid-hours` changes that
  (1–720).
- **Email settings** are `TRESTLE_EMAIL_URL`, `TRESTLE_EMAIL_FROM` and,
  optionally, `TRESTLE_EMAIL_CA_FILE` (see ADR-011).
  - Production accepts only `smtps://` or `smtp://`. The latter requires
    STARTTLS.
  - Percent-encode special characters in the SMTP user and password.
- **Lockout:** five consecutive failed sign-ins lock an account for 15
  minutes. It clears by itself, or with `pilot account enable`. Someone who
  knows an address can keep it locked by guessing; the per-address limit
  slows that.
- **Many people behind one address** (a school network) share the
  per-address limit of `TRESTLE_PASSWORD_ATTEMPTS_PER_MINUTE`. The default
  is 30, all accounts together. Separately, each account (by email, from
  any address) gets 10 attempts a minute; lockout still applies on top. Raise it if a morning rush sees 429s on
  sign-in.
- **No self-service reset yet.** People who forget their password ask an
  operator for a reset link.

## Keys

- **`TRESTLE_CURSOR_KEY` rotation:** restart every replica with the new key.
  Open list cursors become invalid (`invalid_cursor`) and clients restart
  from page one. Replicas must share one key.
- **`TRESTLE_SESSION_KEY` rotation:** CSRF tokens change, and unsafe
  requests get `csrf_failed` until the client re-fetches
  `/api/v1/auth/session`. The first-party clients do that on reload.
  Sessions themselves stay valid; to end them all, see `recover.md`.
- **OIDC client secret:** rotate at the provider, then update the replicas.
  Logins in flight fail and are retried.

## Tuning

- **`TRESTLE_RATE_PER_SECOND` and `TRESTLE_RATE_BURST`** apply per
  verified actor per replica. Before identity is verified, every request
  is charged to its client address: `TRESTLE_ADDRESS_RATE_PER_SECOND`
  (default 20) and `TRESTLE_ADDRESS_BURST` (default 60). The address
  comes from `TRESTLE_CLIENT_ADDRESS_HEADER` (set and overwritten by the
  edge; the API must only be reachable through it; a value that is not a
  single IP address falls back to the peer) or, with
  `TRESTLE_TRUST_PEER_ADDRESS=true`, from the socket peer. Production
  refuses to start without one of the two.
- **`TRESTLE_MAX_IN_FLIGHT`**: beyond this, requests get 503 `overloaded`
  with `Retry-After: 1`.
- **`TRESTLE_STATEMENT_TIMEOUT_MS`**: statements past it are cancelled
  (503 `database_timeout`, with the transaction rolled back).
- **`TRESTLE_DB_POOL_SIZE`**: stay within the database's connection budget
  across all replicas.
