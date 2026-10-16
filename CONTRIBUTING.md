# Contributing to Trestle

Thank you for helping. This file covers how to work on Trestle and the rules
every change has to keep.

## Getting started

- Install Rust (the version in `rust-toolchain.toml` is installed
  automatically by rustup) and run PostgreSQL 13 or newer locally. Your user
  must be able to create databases.
- `make test` runs the Rust suite. Integration tests create and drop
  `trestle_test_*` databases (`TRESTLE_TEST_DATABASE_URL`, default
  `postgres://localhost/postgres`).
- `make lint` runs `cargo fmt --check` and `clippy -D warnings`. Keep both
  clean.
- `make frontend frontend-test` runs the frontend tests (Node 24).
- If you intentionally change a published document, run `make snapshots`
  and review the diff under `examples/pilot/published/`.

## Pull requests

- Keep each change focused, and include tests. A security fix starts with a
  test that fails without the fix.
- Record user-visible changes in `CHANGELOG.md` under `[Unreleased]`.
- Decisions that change the design get an ADR in `docs/adr/`.
- By contributing, you agree that your contribution is dual licensed under
  MIT OR Apache-2.0, as described in the README.

## Rules every change keeps
- Installing a model publishes nothing (I01). Endpoints, forms and writable fields are explicit.
- Inputs are separate types from rows; unknown fields are rejected; ownership (`school_id`,
  `created_by`) comes from trusted context, never the body.
- Authorization happens at execution. Writes recheck membership inside their transaction
  (`lock_membership_role`, `FOR SHARE`). A header, hidden field or metadata doc grants nothing.
- Every query is tenant-scoped in SQL before anything else. Other tenants' records are 404.
- Repositories take an explicit `&mut PgConnection`; they never fall back to a pool.
- A failed COMMIT without a server response is `outcome_unknown`, not a retryable error.
- Applied migrations are immutable (digest-checked). Migration crates never depend on app crates.
  Web startup checks required migrations; it never applies them.
- Problem responses never contain SQL, database messages, secrets or stack traces.
- `DevTokenIdentity` is development-only; production boot refuses it and every other unsafe
  setting (all problems reported at once). Keep those checks.
- Identity yields only an `ActorId` (`trestle-identity`); tenant, role and policy stay in the
  operation. Cookie-authenticated unsafe requests need an allowed `Origin` and `X-CSRF-Token`
  (checked in `Sessions::authenticate`); never add a cookie path that skips it. A request with
  `Authorization` never falls back to its cookie.
- Passwords: only through `trestle_identity::password` (Argon2id via the `argon2` crate). Every
  sign-in failure stays identical and costs one hash verification; never add a response or
  timing difference that reveals whether an account exists or is locked. Link secrets are
  stored as SHA-256, single-use, expiring, and travel in the URL fragment.
- Email: only through a `trestle_mail::Mailer`. Production accepts `smtps://` or STARTTLS-required
  `smtp://`; never log message bodies (they carry sign-in links). The API server sends no mail
  yet: request-time sending needs the outbox (ADR-009, ADR-011).
- Metrics labels stay bounded (route templates, operation IDs, problem codes); never actor,
  tenant or record IDs. The hot path in `metrics.rs` allocates nothing and takes no
  exclusive lock; keep it that way (it shows up directly in the load comparison).
- Models live in apps (`app.model(..)`); migrations come from `pilot makemigrations` and are
  reviewed before commit. `makemigrations --check` must stay clean. Never edit an applied
  migration (digest-checked, data-step source included). Data steps use `HistoricalTx` with
  stable IDs only, never app types.
- Reads use `trestle_core::query::select(scope, ..)`; there is no unscoped variant on purpose.
- Resources are bindings over operations you wrote; CRUD is opt-in, transitions are explicit
  `.action(..)`s. Update/delete return the retrieve representation (If-Match checks it).
  Admin metadata grants nothing: every admin button calls the same operation, which authorizes.
- `frontend/product/src/generated/api.ts` is generated (`make snapshots`); never edit it.
- Every caller goes through the executor (`execute`, `execute_json`, managed routes). Never call
  `Operation::run` directly; that skips the contract, policy and transaction.
- A `ContractDoc` is the only definition of a wire contract. Never hand-write JSON Schema or
  OpenAPI; serde types bound to contracts use `deny_unknown_fields` and only map validated input.
- Published documents (`examples/pilot/published/`) change only through `make snapshots` plus a
  reviewed diff. Policy IDs and private fields never appear in them. `revisions.json` is
  append-only; retain an old form revision only if its submissions still mean the same thing.
- Metadata grants nothing: `/_meta` availability is advisory, every submit re-authorizes, and
  metadata responses are `private, no-store`.
- The browser validator is derived from published schemas; never hand-write per-form rules.
  New field types need server evaluator, schema export, TS decoder/validator and fixtures together.

## Practical notes

- **Stopping processes:** never use `pkill`, `killall` or pattern matching. Start background
  servers with `cmd > log 2>&1 & echo $! > pidfile`, confirm with `ps -p <pid> -o command=`, then
  `kill <pid>`. Pattern matching can hit unrelated processes. Use a spare port (e.g. 8011) so
  a developer's own `make run` on 8000 is never touched.
  In bash, background with `(cd dir && exec env … cmd) &`: without `exec`, `$!` is the
  subshell and killing it orphans the server.
- sqlx futures: don't hold iterator adapters (closures) across `.await`, and prefer
  `Executor::execute(&mut *tx, sql)` over `sqlx::raw_sql(..).execute(..)`. Both break `Send`
  inference (rust-lang/rust#102211). `trestle-historical/src/runner.rs` has a compile-time guard.
- Dependency majors are pinned deliberately (axum 0.8, sqlx 0.8, tower-http 0.6). sqlx 0.9 and
  tower-http 0.7 exist; upgrade as a separate change with the full suite green.
- `.env` is gitignored and holds only development tokens. Never put real credentials in it.
- Lifecycle order is topological with install order breaking ties; only "dependencies first"
  is guaranteed.
- Vite rewrites `new URL(\`…${x}\`, import.meta.url)` into an asset glob; in tests resolve paths
  with `fileURLToPath` instead. In JS, `obj["__proto__"] = …` sets the prototype; test pollution
  with `JSON.parse` input.
- Test passwords must pass the rules: not common, not numeric, and not containing the email's
  local part (`owner…` for owner@example.com fails as `too_similar`).
- A failing integration test leaks its `trestle_test_*` database; drop leftovers with
  `dropdb --force` after checking no test run is active.
