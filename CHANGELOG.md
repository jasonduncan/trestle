# Changelog

All notable changes to Trestle are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
change the API. The migration file format, the CLI's JSON output and its
exit codes are versioned separately (`trestle.plan/1`, `trestle.verify/1`,
`trestle.migrate/1`, manifest `format`) and are kept compatible.

Projects depend on the single `trestle-framework` package, imported as
`trestle`. The other crates (`trestle-core`, `-historical`, `-identity`,
`-protocol`, `-mail`) are internal, pinned exactly, and carry no stability
promise.

## [Unreleased]

### Added
- `Idempotency-Key` receipts (ADR-016, design doc §6.4):
  - `Http::post(..).idempotency_key()` opts a write in;
  - `trestle_core::idempotency::IdempotencyApp` installs the
    `trestle_idempotency` table.

  The receipt is claimed in the write transaction after `recheck`:
  - a repeat with the same key and input replays the committed response
    (`Idempotency-Replayed: true`);
  - a different input is 409 `idempotency_key_reused`;
  - concurrent duplicates are serialized;
  - rolled-back attempts leave no receipt;
  - replays are re-authorized.

  A key sent where none is accepted is 400 `idempotency_unsupported`.
  Other additions:
  - `Invocation::with_idempotency_key` for `Registry::invoke`;
  - `execute_keyed` / `execute_dyn_keyed`;
  - `idempotency::purge_expired`;
  - descriptors and OpenAPI document the header for opted-in operations
    only.
- Operation jobs whose target accepts a key use the job ID as the key, so
  their database effects are exactly once.

## [0.3.0] - 2026-10-17

### Changed
- **Breaking (behavior):** the executor checks every operation's result against its output contract
  before a write commits, and before a read returns, for typed operations
  (`execute`, `execute_json`, `execute_validated`, managed routes) and
  `DynOperation`s (`execute_dyn`) alike. A result that does not satisfy the
  contract (a missing or undeclared field, a wrong type, an out-of-range
  value) is a 500 `internal_error`; a write rolls back. The log names the
  operation, contract, and failing field pointers and codes, never values.
  `execute_dyn` now returns the normalized output. See ADR-012.
- **Breaking:** operation contexts are built from an `Invocation`, not an
  HTTP request (ADR-013):
  - implement `trestle_core::invocation::FromInvocation<S>` instead of
    `FromRequestParts<S>`, reading the actor with `invocation.actor()?` and
    the tenant selection with `invocation.tenant()`;
  - `DynOperation::context_from(&mut Parts, &S)` is now
    `resolve_context(&Invocation, &S)`;
  - states used with managed routes need `Identity: FromRef<S>`.

  HTTP reads only what the context's `describe()` declares: credentials when
  it is `authenticated`, and `Trestle-Tenant` when it lists that header. A
  context that declares neither ignores them.
- `TENANT_HEADER` lives in `trestle_core::invocation`;
  `trestle::intent::TENANT_HEADER` re-exports it.
- The registry owns what it assembles, so applications built from runtime
  data need no `'static` strings (ADR-014):
  - `RegisteredOperation` holds an `OperationDescriptor` and
    `Arc<str>`/`Arc<ContractDoc>` fields, and so do `AppInfo`, `RouteKind`,
    `RegisteredResource` and `CommandContext::name`;
  - `App::id` returns `&str`;
  - `AppBuilder::requires`/`resource`, `Project::new` and `Project::named`
    accept owned or borrowed strings.

  Typed operations are unchanged (`const INFO: OperationInfo`).
  `CommandContext::registry` is an `Arc<Registry<S>>` (was `Rc`), so a
  worker can share it.
  **Breaking** for `DynOperation` implementors: `info()` returns
  `&OperationDescriptor`, and `policy_id()`, `input()` and `output()` borrow
  from the operation.

### Fixed
- `make load` (`scripts/load/compare.sh`) had measured a server that
  rejected almost every request since 0.2.1: it now raises the per-address
  admission limit, and `loadtest compare` refuses to report runs with failed
  requests.
- The `trestle.toml` runtime no longer leaks memory: every assembly leaked
  about 17 KB for the notes example (operation IDs, contracts, the parsed
  intent), and every `policy_id()` call leaked a string. The intent is now
  shared and freed with the registry.

### Added
- Durable jobs and a transactional outbox (ADR-015, guide: `docs/guide/jobs.md`):
  - `trestle_core::jobs::JobsApp` installs the `trestle_jobs` table;
  - `jobs::enqueue(conn, NewJob::operation(..) | NewJob::handler(..))` runs
    in the operation's transaction;
  - `AppBuilder::job(kind, handler)`;
  - `Worker` with fenced claims, leases, retries with backoff, bounded
    attempts and a bounded drain;
  - `jobs::status` and `jobs::purge_finished`.

  Operation jobs are re-authorized on every attempt. `serve --jobs`
  (`TRESTLE_JOBS`, `TRESTLE_JOB_CONCURRENCY`) runs a worker in the server
  process.
- `trestle_operation_output_invalid_total{operation}`: results refused
  because they do not satisfy the output contract.
- Invoking operations without HTTP (ADR-013):
  - `Invocation::for_actor(..).with_tenant(..)` for trusted callers (jobs,
    operator commands, tests);
  - `operation::invoke` / `invoke_dyn`, and `Registry::invoke(state, id,
    &invocation, &input)` by stable operation ID;
  - `TestApp::invoke`.

  They resolve the context and run the executor exactly as managed routes
  do, and are not charged to admission.
- `invocation::Resolved<C>`: an Axum extractor for hand-written handlers that
  resolves `C` the same way.

## [0.2.1] - 2026-10-16

### Changed
- Unauthenticated traffic per client address now defaults to 20 requests/s
  with a burst of 60, instead of ten times the per-actor limits.
  `AdmissionConfig::address_rate_per_second` and `address_burst` are now
  plain `f64`; set them with `TRESTLE_ADDRESS_RATE_PER_SECOND` and
  `TRESTLE_ADDRESS_BURST`.
- Password sign-in attempts per client address default to 30/min (was 60).

### Added
- Per-account password sign-in limit: 10 attempts a minute per email, from
  any address, whether or not the account exists
  (`PasswordPolicy::account_attempts_per_minute`). Lockout still applies.

## [0.2.0] - 2026-10-15

### Added
- Licensed under MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`,
  `NOTICE`); `cargo deny` checks dependency licenses.
- `SECURITY.md` and `CONTRIBUTING.md`; a README for newcomers. The pilot
  example's material moved to `examples/pilot/README.md`.
- CI runs the frontend tests and typecheck.

### Changed
- Prepared for crates.io (not yet published): the facade package is
  `trestle-framework` with `[lib] name = "trestle"`, so imports, the binary,
  `trestle.toml` and `TRESTLE_*` are unchanged. Internal crates use
  `version = "=0.2.0"` path dependencies; every publishable crate has a
  README, keywords, categories and an `include` list; docs.rs builds with all
  features. The workspace version is 0.2.0.
- `trestle new` depends on the crates.io release by default
  (`trestle = { package = "trestle-framework", version = "0.2" }`);
  `--git`/`--tag`/`--trestle-path` override it. New projects declare
  `rust-version` instead of pinning a toolchain.

### Fixed
- The scaffold template README shows the right `trestle tenant grant` flags.
- The migrations integration test works against a PostgreSQL that needs a
  password.

## [0.1.0] - 2026-10-15

The first release.

### Added
- **Foreign apps**: `Project::foreign_apps` / `MigrationSet::with_foreign_apps`
  leave ledger rows of apps that live in another project alone (`migrate`
  no longer refuses them as "unknown applied"); `verify --json` lists them
  under `foreign`.
- **`trestle` facade crate** (F1):
  - `trestle::project().run()` is the project binary's command line
    (`serve`, `check`, `migrate`, `makemigrations`, `openapi`, ...), in the
    style of Django's `manage.py`.
  - `trestle::prelude`, a default `State`, and `App` without a type
    parameter.
- **Runtime migration loading** (F2):
  - `migrations/<app>/<id>.{json,sql}` are read at runtime, or embedded with
    `trestle::embed_migrations!`.
  - Data steps are registered by name with `app.data_step(...)`.
  - Digests are compatible with existing ledgers.
- **Intent file `trestle.toml`** (F3):
  - It declares models, CRUD endpoints, roles and workspace tenancy, and is
    read at runtime.
  - Rust handlers and hooks are linked by name, with startup errors that
    carry a line number.
  - New pieces: a row accessor, `If-Match` concurrency, and the
    `tenant create/grant/revoke` commands.
- **`trestle` wrapper and project template** (F4):
  - The `trestle-cli` crate builds the `trestle` command, and
    `trestle new NAME` scaffolds an intent-based project.
  - Password sign-in is on by default. Google/OIDC is opt-in through
    `[auth] oidc`, which names environment variables, never secrets.
  - `trestle::testing::TestApp` (feature `testing`) supports a project's own
    tests.
- **Non-atomic migrations** (F5):
  - `"atomic": false` runs one statement per step, separated by
    `-- trestle:step` lines.
  - The ledger records `step` and a `partial` status, and a rerun resumes
    where it stopped.
  - An invalid index left by a failed `CREATE INDEX CONCURRENTLY` is dropped
    before the retry.
  - Manifests can set `lock_timeout` and `statement_timeout`.
- **Reconciler contract** (F6):
  - `migrate --plan --json` prints `trestle.plan/1` with an auto/approval
    classifier.
  - `migrate --approve <digest>`, `migrate --auto`, `--backup-ref` and
    `--lock-timeout`.
  - `verify --json` prints `trestle.verify/1`.
  - The exit codes are stable (0–7, 10).
- **CI** (F7):
  - fmt and clippy.
  - Tests on PostgreSQL 13 and 17.
  - Pilot `makemigrations --check`, replay from empty, and upgrade from the
    last tag.
  - A `trestle new` scaffold job.
  - `cargo deny`.
  - A release consistency check on tags.

### Changed
- The pilot product is now an example: `examples/pilot`,
  `examples/pilot-migrations`, `examples/loadtest`. Its Smart Folder backend
  moved to its own repository.
- In production, plain `migrate` applies only plans the classifier marks
  `auto`. Anything else needs `--approve <digest>`.
- `serve` refuses to start on schema drift or a partly applied migration.

[Unreleased]: https://github.com/jasonduncan/trestle/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/jasonduncan/trestle/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/jasonduncan/trestle/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/jasonduncan/trestle/releases/tag/v0.2.0
[0.1.0]: https://github.com/jasonduncan/trestle/releases/tag/v0.1.0
