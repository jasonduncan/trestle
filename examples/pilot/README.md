# The pilot example

`examples/pilot` is the example school product Trestle was built out of:
schools as tenants, units and lessons, accounts, audit. It is not published
and not a product; it is the reference consumer that CI migrates, replays and
tests on every change. Paths below are relative to the repository root.

## Running the pilot

Requires Rust (pinned in `rust-toolchain.toml`) and a running PostgreSQL 13 or newer. Run these from the repository root.

```sh
cp .env.example .env      # DATABASE_URL, dev tokens
make db                   # start local PostgreSQL, create trestle_dev
make migrate seed         # apply history, add two schools and three users
make run                  # API on http://127.0.0.1:8000
make frontend             # once: npm install in frontend/
make web                  # React app on http://127.0.0.1:5173 (proxies /api and /_meta)
make admin                # admin shell on http://127.0.0.1:5174/admin/ (sign in as olivia, an owner)
```

```sh
curl -i -X POST localhost:8000/api/v1/lessons \
  -H 'Authorization: Bearer alice-dev-token' \
  -H 'Trestle-Tenant: 11111111-1111-4111-8111-111111111111' \
  -H 'Content-Type: application/json' \
  -d '{"title": "Intro to Ratios", "grade_level": 6}'
```

Development users: `alice` (teacher, Northside `1111…`), `bob` (teacher,
Southside `2222…`), `carol` (viewer, Northside), `olivia` (owner, Northside;
sees the admin). Tokens are `<name>-dev-token`.

## How an app is put together

```rust
// examples/pilot/src/lessons/mod.rs
impl App<AppState> for LessonsApp {
    fn id(&self) -> &'static str { "lessons" }

    fn register(&self, app: &mut AppBuilder<AppState>) {
        app.requires("schools").requires("audit");
        app.schema(pilot_migrations::LESSONS_0001);
        app.operation(CreateLesson)
            .http(Http::post("/api/v1/lessons").created("/api/v1/lessons/{id}"));
        app.operation(GetLesson).http(Http::get("/api/v1/lessons/{id}"));
        app.form(create_form());
    }
}
```

An operation names its input and output contracts, its request context, its
policy and its behavior. The executor runs every caller through the same
sequence: validate against the contract → `Policy::admit` → (writes) begin a
transaction → `Policy::recheck` → `run` → commit, classifying an unknown commit
outcome. A hand-written Axum handler gets the same guarantees by calling
`trestle_core::operation::execute_json`.

Contracts are defined once (`ContractDoc`). The same definition validates
requests, produces JSON Schema/OpenAPI, and feeds interaction descriptors.

## Commands

| Command | Needs DB | What |
|---|---|---|
| `pilot serve` | yes | HTTP API; refuses to start if required migrations are missing or, in production, if the configuration is unsafe (see `deploy/pilot.env.example`) |
| `pilot identity actor-id --issuer I --subject S` | no | the actor ID for a provider account |
| `pilot identity revoke --actor A` | yes | end every browser session of an actor |
| `pilot provision school\|member\|remove-member` | yes | operator provisioning of tenants and roles (audited) |
| `pilot account invite\|reset-link [--send]` | yes | one-time set-password links, printed or emailed |
| `pilot account disable\|enable\|list` | yes | stop or restore an account (enable also clears a lockout); list with status |
| `pilot mail test --to ADDRESS` | no | send a test message with the configured backend |
| `pilot migrate [--plan]` | yes | apply committed history (also `pilot-migrate`) |
| `pilot seed-dev` | yes | development schools and memberships |
| `pilot check` (`make registry`) | no | assemble and validate the registry; print digests |
| `pilot inspect apps\|routes\|operation <id>` | no | describe the composition |
| `pilot contracts export` | no | Contract IR, digests, JSON Schema |
| `pilot interactions export` | no | `trestle.interactions/1.0` form descriptors |
| `pilot openapi export` | no | OpenAPI 3.1.1 |
| `pilot sdk export` | no | generated TypeScript client |
| `pilot makemigrations [--check] [--name N]` | no | plan models vs history; write `examples/pilot-migrations/migrations/<app>/<id>.{json,sql}` |
| `pilot makemigrations --empty --app A --data STEP --name N` | no | data-only migration running `pilot_migrations::data::STEP` |
| `pilot schema show [--history]` | no | desired (models) or replayed history |
| `pilot schema verify` | yes | database catalog vs the history its ledger says is applied |

Exports take `--audience product|admin` and print canonical JSON.

## Endpoints

| Method | Path | Notes |
|---|---|---|
| GET | `/api/v1/lessons` | `lessons.list`: `?grade_level&status&unit_id&limit&cursor` → `{items, next_cursor}` |
| POST | `/api/v1/lessons` | `lessons.create`: `{title, grade_level, subtitle?, unit_id?}`; 201 + `Location` + `ETag` |
| GET | `/api/v1/lessons/{id}` | `lessons.get` with `ETag: "lessons.view:<version>"`; other schools' lessons are 404 |
| PATCH | `/api/v1/lessons/{id}` | `lessons.update`: `If-Match` required (428/412); absent = unchanged, `null` = clear |
| DELETE | `/api/v1/lessons/{id}` | `lessons.delete`: `If-Match` required; 204 |
| POST | `/api/v1/lessons/{id}/actions/publish` | `lessons.publish`: `{expected_version}`; 409 `stale_version` / `invalid_transition` |
| GET | `/api/v1/lessons/export.csv` | hand-written Axum route (unmanaged, waived) |
| GET/POST | `/api/v1/units`, `GET /api/v1/units/{id}` | `units.list/create/get` |
| GET | `/_meta/v1/admin/manifest`, `/_meta/v1/admin/interactions/{id}` | admin profile (owners); `private, no-store` |
| GET | `/_meta/v1/manifest` | Interactions the caller can see, with advisory availability; `private, no-store` |
| GET | `/_meta/v1/interactions/{id}` | One descriptor (`trestle.interactions/1.0`) plus availability; `private, no-store` |
| GET | `/api/v1/me/schools` | `schools.mine`: the caller's memberships (no tenant header) |
| GET | `/api/v1/auth/login?return_to=/path` | Browser login (OIDC authorization code + PKCE); 303 to the provider |
| GET | `/api/v1/auth/callback` | Provider redirect target; creates the session, 303 to `return_to` |
| GET | `/api/v1/auth/session` | `{actor, csrf_token, expires_at}` for the session cookie; 401 without one; 404 when login is not configured |
| POST | `/api/v1/auth/logout` | Revokes the session (origin + CSRF checks); 204 |
| GET | `/api/v1/auth/methods` | `{password, oidc}`: what the sign-in screen offers; 404 without browser sign-in |
| POST | `/api/v1/auth/password/login` | `{email, password}` → session; identical `login_failed` for every failure |
| POST | `/api/v1/auth/password/inspect` | `{token}` → `{email, purpose, expires_at}` for a live link |
| POST | `/api/v1/auth/password/set` | `{token, password}` → session; the link works once; ends other sessions |
| POST | `/api/v1/auth/password/change` | `{current_password, new_password}` (session + CSRF); ends other sessions; 204 |
| GET | `/healthz` | Liveness (system route) |
| GET | `/readyz` | Database reachable and every app's required migrations applied (system route) |

Every API request needs credentials: a development token or provider JWT as
`Authorization: Bearer …`, or the browser session cookie with `X-CSRF-Token`
on unsafe methods. Tenant-scoped routes also need `Trestle-Tenant: <school id>`.
The tenant header only selects among the caller's memberships; it grants nothing.
Errors are `application/problem+json` with a stable `code`, a `request_id`, and
field `errors` addressed by JSON Pointer.

## Tests

`make test` runs the Rust suite; `make check` adds rustfmt, clippy and the
frontend tests (`make frontend-test`). `make demo-m2` runs the real-browser
demonstration across releases A (a local tag, `RELEASE_A=<ref>`, default `m2-release-a`) and B, and
`make demo-m4` the product/admin demonstration, `make demo-m5` (`OLD_TAG=<ref>`, default `m4-complete`) the
rolling-deploy and production-identity demonstration, and `make demo-accounts`
the password-account flow, all using the installed
Chrome headlessly with a throwaway profile. Integration
tests need PostgreSQL: each creates a throwaway `trestle_test_*` database
(`TRESTLE_TEST_DATABASE_URL`, default `postgres://localhost/postgres`), replays
the committed history into it, and drops it afterwards.

If a published document changes intentionally, run `make snapshots` and review
the diff under `examples/pilot/published/`.

## History: milestones M0–M5

M0–M5 are built and demonstrated. The M5 exit criterion, explicit pilot
approval, belongs to the product owner. External
security review (§12.4) and the choice of identity provider are still open.

**Examples.** `examples/pilot` (with `examples/pilot-migrations` and
`examples/loadtest`) is the example school product the framework was built
out of: schools as tenants, units and lessons, accounts, audit. It is not
published and not a product; it is the reference consumer that CI migrates,
replays and tests on every change. Each crate under `crates/trestle/examples/`
is a smaller, single-file starting point. An app that used
to live in pilot moved to another project; pilot lists `smartfolder` as a
foreign app so databases that ran it keep working (`Project::foreign_apps`, see `docs/guide/reconciler.md`).

The design builds the framework out of a real product (the `pilot` example) in
milestones M0–M5 (§18).

**M0**, one real protected API path: Axum/PostgreSQL, a replayable migration
history, an identity adapter, tenant scoping, problem+json errors, and one
transaction with a proven rollback and constraint failure.

**M1**, extracted from that path without changing its behavior (the M0 API tests
run unmodified against it):

| M1 requirement | Where |
|---|---|
| Explicit App/Project registry, pure assembly | `crates/trestle-core/src/registry/mod.rs`; apps in `examples/pilot/src/lib.rs` |
| Registered `lessons.create` (and `lessons.get`) | `examples/pilot/src/lessons/operations.rs` |
| Executor extracted from the M0 path | `crates/trestle-core/src/operation.rs` |
| Portable constraints and Contract IR | `crates/trestle-protocol/src/contract.rs` |
| JSON Schema / OpenAPI 3.1.1 export | `crates/trestle-core/src/registry/export.rs`; snapshots in `examples/pilot/published/` |
| Hand-authored form bound to the operation | `examples/pilot/src/lessons/mod.rs` (`create_form`) |

| M1 demonstration | Test |
|---|---|
| Adding a second app validates dependencies | `examples/pilot/tests/registry.rs` |
| Export works with networking disabled (macOS sandbox, with a control) | `examples/pilot/tests/offline.rs` |
| Contract rejects input identically via managed and hand-written routes, and the published schema agrees | `examples/pilot/tests/conformance.rs` + `fixtures/contracts/` |
| Unknown field references fail checks | `crates/trestle-core/src/registry/tests.rs`, `examples/pilot/tests/registry.rs` |
| Missing policy fails | compile error, proven by a `compile_fail` doctest on `Operation` |

**M2**, one runtime-generated React form:

| M2 requirement | Where |
|---|---|
| Metadata endpoints, versioned envelope | `/_meta/v1/manifest`, `/_meta/v1/interactions/{id}` (`crates/trestle-core/src/registry/meta.rs`) |
| React `OperationForm`, local widget registry, error mapping, feature checks | `frontend/interaction-react/` |
| Stale-interaction handling | `Trestle-Interaction-Revision` → 409 `interaction_stale`; retained revisions + `examples/pilot/published/revisions.json` |
| Product app using it | `frontend/product/` (same-origin Vite app) |

| M2 demonstration | Evidence |
|---|---|
| Added optional field shown by the existing bundle, no rebuild | `docs/evidence/m2-demo.json` + screenshots (`make demo-m2`, real Chrome) |
| Old form kept open across a deploy submits (retained revision) or gets 409 | same run; `examples/pilot/tests/meta.rs` |
| Permission revoked between load and submit is rejected | same run; `meta.rs`; component test |
| No duplicate client validation; no client/server drift | browser validator derived from the published schema passes all 26 shared fixtures |

**M3**, models and migrations:

| M3 requirement | Where |
|---|---|
| Stable schema IDs, Model declaration (explicit builder) | `crates/trestle-historical/src/schema.rs`; pilot models in each app |
| Desired/history diff → reviewable artifacts | `planner.rs`, `artifact.rs`; `pilot makemigrations [--check]` |
| Historical SDK for data migrations | `crates/trestle-historical/src/data.rs` (`HistoricalTx`) |
| Ledger/recovery, catalog verification | `runner.rs`, `verify.rs`; `pilot schema verify` |
| Small typed query surface (scope required) | `crates/trestle-core/src/query.rs`; `lessons::repository` |

| M3 demonstration | Test |
|---|---|
| Add/backfill/rename replay after current-type removal; fresh and upgrade converge | `crates/trestle-historical/tests/evolution.rs` |
| Concurrent runners serialize | `crates/trestle-historical/tests/runner.rs` |
| A presentation change creates no migration | `examples/pilot/tests/schema.rs` |
| Unsupported destructive changes block generation | `schema.rs`, planner unit tests |

**M4**, resources, SDK and admin:

| M4 requirement | Where |
|---|---|
| Narrow DRF-style Resource bindings | `crates/trestle-core/src/registry/resource.rs`; `units` and `lessons` apps |
| List/create/update/delete + domain actions | `lessons.list/create/get/update/delete/publish`, `units.list/create/get` |
| Pagination/filter contracts | `pagination.rs` (`Page<T>`, HMAC cursors bound to the query), allowlisted filter fields |
| Generated TypeScript client | `pilot sdk export` → `frontend/product/src/generated/api.ts` (snapshot-checked) |
| Scoped relation picker | `relation` widget + `units.list`; server revalidates `unit_id` |
| Optional admin (backend profile + React shell) | `/_meta/v1/admin/*`, `frontend/admin` |

| M4 demonstration | Evidence |
|---|---|
| Second resource without hand-wired HTTP | `units` (4 binding lines); `docs/evidence/m4-developer-steps.md` |
| Custom Axum handlers still work | `GET /api/v1/lessons/export.csv` (unmanaged, waived, reuses `lessons.list`) |
| Admin and product invoke the same publish operation | `docs/evidence/m4-demo.json` (real Chrome), `tests/resources.rs` |
| Patch null/missing, stale edits, forbidden fields, hidden relation targets | `examples/pilot/tests/resources.rs` |

**M5**, limited pilot and production hardening:

| M5 requirement | Where |
|---|---|
| Production identity and session profile | `crates/trestle-identity` (OIDC login with PKCE, server-side sessions, origin + CSRF checks, JWT bearer); ADR-005 |
| Production boot checks | `examples/pilot/src/server.rs`: no dev tokens, a provider, keys, https, database TLS |
| Rate limits and admission control | `crates/trestle-core/src/admission.rs` (429 `rate_limited`, 503 `overloaded`) |
| Observability | `crates/trestle-core/src/metrics.rs`, internal `/metrics` (`TRESTLE_METRICS_BIND`); bounded labels |
| Edge configuration | `deploy/Caddyfile` (checked locally by `make drill-edge`; not yet on a real host), `deploy/pilot.service`, `deploy/pilot.env.example` |
| Recovery runbooks and compatibility matrix | `docs/runbooks/`, `docs/compatibility.md` |
| Security review | `docs/security-review.md` (2026-10-20 review, findings fixed in 0.7.x; accepted by the owner as the §12.4 external review on 2026-10-21) |
| Jobs and outbox | not needed by the pilot; ADR-009 records the deferral |

| M5 demonstration | Evidence |
|---|---|
| Old browser bundle, new API replica, additive schema change and in-flight edit behave as documented | `docs/evidence/m5-demo.json` + screenshots (`make demo-m5`, real Chrome, the M4 release built from its tag) |
| Load test with matched work and a hand-written Axum baseline | `docs/evidence/m5-load.json` (`scripts/load/compare.sh`); ADR-010 |
| Restore from backup and a forward-repair drill | `docs/evidence/m5-restore.json` (`scripts/drills/restore.sh`) |
| Database TLS | `docs/evidence/m5-db-tls.json` (`scripts/drills/db-tls.sh`) |

**After M5**, Django-style password accounts and outgoing email:

| Piece | Where |
|---|---|
| Password sign-in (Argon2id, Django's password rules, lockout, per-address limit) | `crates/trestle-identity/src/password.rs`; ADR-005 addendum |
| Operator-issued invite and reset links | `pilot account invite\|reset-link [--send]`; `/set-password#token=…` in the product app |
| Email through SMTP (TLS or required STARTTLS), file backend for development | `crates/trestle-mail`; `pilot mail test`; ADR-011 |
| Sign-in, set-password and change-password screens | `frontend/interaction-react/src/auth.tsx` |
| Real-browser demonstration | `docs/evidence/accounts-demo.json` (`make demo-accounts`) |

Decisions are recorded in `docs/adr/` (001–011).

