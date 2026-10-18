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

## [0.5.0] - 2026-10-18

### Added
- The intent as a value (ADR-030):
  - `trestle::intent::IntentDoc` (with `ProjectDoc`, `TenancyDoc`, `ModelDoc`,
    `FieldDoc`, `EndpointDoc` and `AdminDoc`) is the `trestle.toml` schema as
    public data;
  - `to_toml()` renders the canonical file, and `validate(name)` checks it
    with the same `intent::parse` the file goes through;
  - `Project::intent_doc(name, doc)` serves a document exactly like its
    rendering, with handlers, hooks, migrations and tenancy;
  - `trestle::intent::toml` re-exports the `toml` crate for `default`, `set`
    and `when` values.
- Rust operations in intent projects (ADR-031):
  - `trestle::intent::Permission` checks a `[roles]` permission the way
    generated operations do: `resolve`, `admit`, and `recheck` in the
    transaction; it is also a `DynPolicy`;
  - `Ctx::run(&resolved, conn, &ctx, f)` gives a `DynOperation` the intent's
    scoped data handle on its own connection, writable only in a write
    transaction;
  - `Project::intent_app(|resolved| app)` installs an app built from the
    linked intent;
  - generated operations and `[[admin]]` use the same `Permission` and
    `Ctx::run`.

### Tests
- A runtime-built write is now covered under the schema fence
  (`trestle-core/tests/fence.rs`), with a stale context rechecked in its
  transaction, and behind `ResourceOp` and `admin_dyn`
  (`tests/intent_permission.rs`).

## [0.4.0] - 2026-10-17

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
- A schema fence for older builds (ADR-017). A migration whose manifest
  says `"fence": true` is recorded in `trestle_schema_fence` when applied.
  From then on, a build that lacks it gets 503 `schema_fenced` with nothing
  written, for executor writes, `Registry::invoke` and job claims.
  - **Write path:** the check travels with `BEGIN`, so it adds no round
    trip.
  - **Migration side:** the fence migration waits for writes already under
    way, and writes that arrive meanwhile wait for it.
  - **Readiness and startup:** `/readyz` reports `schema_fenced`, and
    `serve` refuses to start a fenced build.
  - **Output:** `verify` lists `fenced_by`, and `trestle.plan/1` marks
    `fence: true` with the approval reason `fence`.
  - **New API:** `trestle_core::fence` (`install`, `begin`, `check`) and
    `trestle_historical::fence` (`BuildFence`, `status`, `fenced_by`).

- Declared operation effects (ADR-018):
  - **Typed operations** declare `const EFFECTS: Option<EffectsInfo>`;
    runtime-built ones use `OperationDescriptor::effects`. The four lists
    are `reads`, `writes`, `emits` and `egress`.
  - **At assembly,** every named model, operation and job kind must exist,
    and a read may not declare writes or emits.
  - **At run time,** a write that declared its effects may enqueue only
    what its `emits` lists.
  - **Shown by** `inspect operation`, and by the new
    `inspect effects [--model ID]`.
  - **Generated `trestle.toml` CRUD** declares its model, except where a
    hook runs. The pilot declares effects on every operation.

- Plans assert their source state and report their impact (ADR-019):
  - **Preconditions:** a manifest may list
    `"preconditions": [{"name", "violations"}]`, each a `SELECT` counting
    rows that break it. The runner evaluates them before a migration
    changes anything (exit 4 on a violation), and `migrate --plan` reports
    them.
  - **Impact:** `trestle.plan/1` steps carry `impact`: estimated rows, plus
    exact counts of null rows before `SET NOT NULL`, duplicate groups
    before a unique constraint, rows to backfill, and values or rows
    deleted. A step that would fail is `blocked`. All of it is measured
    read-only with a 5 s timeout.

- Batched data steps (ADR-020): `DataStep::batched(name, source, run)`
  runs a `BatchFn` one bounded batch at a time.
  - **Transactions:** each batch commits in its own transaction together
    with its checkpoint, in the ledger's new `checkpoint` column.
  - **Failure and rerun:** a failure rolls back only that batch and leaves
    the migration `partial`. A rerun resumes after the last committed batch,
    so every row is processed exactly once.
  - **Creating one:** `makemigrations --empty --batched`, or
    `artifact::empty_batched`, writes the data-only `"atomic": false`
    migration.

- `makemigrations --online` (`PlanOptions::online`, ADR-021) splits a
  change into two migrations:
  - **Atomic:** everything else, with checks on existing tables added
    `NOT VALID`.
  - **Non-atomic `_online`:** `CREATE INDEX CONCURRENTLY IF NOT EXISTS` for
    each index on an existing table, then `VALIDATE CONSTRAINT`.

  New `StateOp::ValidateConstraint` and `AlterCheck::not_valid`; the field
  is left out when false. A `NOT VALID` check, and a `CONCURRENTLY` index
  in a non-atomic migration, are no longer `existing_table_lock`.

- Portable scalar types in contracts (ADR-022): `Field::decimal(name,
  precision, scale)`, `Field::date`, `Field::instant` and `Field::duration`.
  - **Wire form:** each is a JSON string with one syntax. Decimals are never
    JSON numbers, and instants are normalized to UTC.
  - **Everywhere at once:** the server, the JSON Schema export and the
    browser validator agree on `fixtures/contracts/scalars.json`.
  - **Forms:** new widgets `decimal`, `date`, `datetime` and `duration`,
    which require `forms.decimal@1` or `forms.temporal@1`.
  - **SDK:** they are `string` in the generated TypeScript.

- Columns and `trestle.toml` fields for the scalar types (ADR-023):
  - **Columns:** `Column::decimal(key, precision, scale)` (`numeric(p,s)`),
    `Column::date`, `Column::duration` (`interval`), and `timestamptz`.
  - **`verify`** compares numeric precision and scale.
  - **`HistoricalTx`** reads and writes all four in their contract forms;
    it used to refuse `timestamptz`.
  - **`trestle.toml`:** fields may be `decimal` (`precision`, `scale`),
    `date`, `instant` and `duration`. CRUD servers return them as strings,
    with instants in UTC.

- `trestle.toml` relations and paging (ADR-024):
  - **References:** `type = "ref", to = "<model id>"` fields are indexed
    foreign keys. Every create and update checks that a non-null ref names
    a row of that model in the caller's tenant, and locks it
    `FOR KEY SHARE`. Otherwise the write is a 422 `not_found` on the field.
  - **Deletes:** deleting a referenced row is 409 `still_referenced`.
  - **Paging:** lists take a signed `cursor` and return `next_cursor`, like
    Rust lists.

- `trestle.toml` domain actions (ADR-025): an endpoint with `set`, `when`
  or `accept` (and no handler) is a `POST …/{id}/<verb>` transition.
  - **Order of checks:** it locks the row in the caller's tenant, then
    requires `If-Match`, then requires a `when` state (409 `invalid_state`
    otherwise).
  - **Writes:** `set` constants (`"now"` for instants) and the accepted
    fields.
  - **Response:** the row, with an `ETag`.

- Generated forms for `trestle.toml` (ADR-026). `forms = true` on a CRUD
  endpoint or an action publishes its create, update or action forms.
  - **Widgets** follow field types, and refs become relation pickers over
    the target's list.
  - **Labels** come from `label` on fields, or `label` on models for
    pickers.
  - **Checked:** fields that can't be shown are refused when the file is
    read.

- Money and references (ADR-027):
  - **References:** `Field::reference(name, model)` is a uuid on the wire
    that records its target model in the IR. `trestle.toml` refs use it.
  - **Money:** `Field::money(name, precision, scale).currencies(..)` is
    `{"amount", "currency"}`, with errors at `/field/amount` and
    `/field/currency`.
  - **Agreement:** the server, JSON Schema and browser agree on
    `fixtures/contracts/composites.json`.
  - **Widget:** `money`, which requires `forms.money@1`.
  - **Storage:** in `trestle.toml` (`type = "money"`), money is stored as
    `<field>_amount` and `<field>_currency`.
- Tagged unions (ADR-028):
  - **Contracts:** `Field::union(name, tag, &[(variant, &contract), ..])`
    takes one of several shapes, named by its tag member
    (`{"method": "card", "last4": "4242"}`).
  - **Schema and errors:** JSON Schema `oneOf` with `const` tags. Member
    errors carry the union's pointer (`/payment/last4`).
  - **Agreement:** the server, JSON Schema and browser agree on
    `fixtures/contracts/unions.json`.
  - **SDK:** a discriminated union on the tag.
  - **Widget:** `union`, which requires `forms.union@1` and shows the chosen
    variant's fields.
- **`OperationForm`:** errors under a field (`/price/amount`) now show on
  that field, prefixed with the member, instead of only in the summary.
- Admin views for `trestle.toml` apps (ADR-029):
  - **Resource verbs** take a typed `Operation` or an
    `Arc<dyn DynOperation<S>>` (`ResourceOp`). `action_at` takes a binding
    you choose.
  - **`DynPolicy`** and `admin_dyn(view, policy)` publish an admin profile
    from data.
  - **`[[admin]]`** in `trestle.toml` (model, permission, title, columns,
    filters, actions) shows a model in the admin shell, with generated
    admin forms and its actions as buttons. Every intent CRUD endpoint is a
    resource.
  - **Admin actions** bound with `If-Match` are described with `ifMatch`,
    and the shell sends the row's ETag.
  - **The admin shell** shows money as "12.50 USD".

### Changed
- **Breaking:** `RegisteredResource::admin_policy()` returns `Option<&str>`
  (was `&'static str`). Resource verbs take `impl ResourceOp<S>` instead of
  a generic `O: Operation`; calls are unchanged.
- **Fixed:** runtime-built operations (`dyn_operation`) have an
  availability function, so `/_meta` serves forms generated for
  `trestle.toml` apps (ADR-026 published them but never served them).
- **Breaking (intent format):** an endpoint's `order` must name a required
  field or `created_at`. Intent list contracts gain `cursor`, and pages
  gain `next_cursor`.
- **Breaking:** `ColumnType` has `Numeric`, `Date` and `Interval` variants,
  and `ColumnType::sql()` returns `Cow<'static, str>`.
- **Breaking:** `DataStep::run` is a `DataRun` (`Once` or `Batched`).
  Build one-shot steps with `DataStep::once(name, source, f)`, or write
  `run: DataRun::Once(f)`. Every `migrate` run adds the `checkpoint` column
  to an existing ledger.
- **Breaking (behavior):** `migrate` checks that the catalog matches
  applied history before applying anything, and exits 6 (`HistoryError::Drift`)
  if it doesn't. `--allow-drift` (`RunConfig::allow_drift`) overrides it.
  `migrate --plan` lists the differences under `database.drift`.
  `reconcile::apply` takes `allow_drift`.
- **Breaking (behavior):** with `trestle_core::db::read_only_reads()` on,
  `Mode::Read` operations run in a `BEGIN READ ONLY` transaction, so a read
  that writes fails with a 500 instead of committing (ADR-018). It is on by
  default in debug builds, which includes tests. Release builds opt in with
  `serve --read-only-reads` (`TRESTLE_READ_ONLY_READS`), at about a fifth of
  read throughput. `DynConn` gains a `ReadOnly` variant.
- `migrate` creates the `trestle_schema_fence` table and the
  `trestle_fence_v1` function on every run, like the ledger. `serve` refuses
  to start until they exist, so run `migrate` once after upgrading.
- `RunOptions` and `Manifest` have a `fence` field. Manifests that leave it
  out are unchanged, digests included.
- Job workers claim inside a transaction, so a fenced build claims nothing.

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

[Unreleased]: https://github.com/jasonduncan/trestle/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/jasonduncan/trestle/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/jasonduncan/trestle/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jasonduncan/trestle/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/jasonduncan/trestle/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/jasonduncan/trestle/releases/tag/v0.2.0
[0.1.0]: https://github.com/jasonduncan/trestle/releases/tag/v0.1.0
