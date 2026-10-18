# Trestle

API-first Rust application framework (Django/DRF-style composition, REST/JSON only, no HTML).
Built product-first: the `pilot` crate is a real application, and framework pieces are extracted
into `trestle-*` crates only after the plain-Rust version works (design doc §17.3).

## Read first
1. `README.md`: status, layout, quick start.
2. `docs/design/Rustjango_API_First_Technical_Design_v0.2.md`, especially §2 (invariants I01–I14),
   §6.2 (command sequence), §7.2 (error contract), §14 (migrations) and §18 (milestones M0–M5).

## The name is Trestle
The design doc predates the rename and says "Rustjango" throughout. Translate, never copy:

| Doc says | Use |
|---|---|
| Rustjango, `rustjango` | Trestle, `trestle` |
| `rj-core`, `rj-protocol`, `rj-historical` | `trestle-core`, `trestle-protocol`, `trestle-historical` |
| `rustjango.interactions/1.0` | `trestle.interactions/1.0` |
| `RJ-Interaction-Revision` | `Trestle-Interaction-Revision` |
| `urn:rustjango:problem:*` | `urn:trestle:problem:*` |
| `rj.portable@1` | `trestle.portable@1` |

The design doc filename keeps its original name on purpose (it is the v0.2 artifact).

## Current milestone
M0–M5 are built (2026-09-26); the M5 exit, **pilot approval, belongs to the project owner**
and is not self-granted. Open items before a pilot:
choose how people sign in (password accounts are built; an OIDC provider is optional and only
mock-tested), send through the real SMTP relay, get external security review (§12.4), and
validate the edge config (`deploy/Caddyfile`) on a real host. Do not claim general
maturity or benchmark leadership. ADRs: 001–027 (015 reopens 009: jobs and outbox).

## Non-negotiables
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
  setting (`examples/pilot/src/server.rs`, all problems reported at once). Keep those checks.
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
- `migrate` refuses to apply over drift (ADR-019); never add `--allow-drift` to a script or
  default. Preconditions and impact counts run read-only; keep any new count bounded by the
  plan's statement timeout and out of `plan_digest`.
- Models live in apps (`app.model(..)`); migrations come from `pilot makemigrations` and are
  reviewed before commit. `makemigrations --check` must stay clean. Never edit an applied
  migration (digest-checked, data-step source included). Data steps use `HistoricalTx` with
  stable IDs only, never app types. Large backfills are `DataStep::batched` (ADR-020): one
  bounded batch per call, checkpoint returned, idempotent per row; its progress must stay in
  the batch's own transaction.
- Reads use `trestle_core::query::select(scope, ..)`; there is no unscoped variant on purpose.
- Intent `ref` writes check the target row is in the caller's tenant (`check_references`, ADR-024)
  and report another tenant's row exactly like a missing one; never add a write path around it.
- Resources are bindings over operations you wrote; CRUD is opt-in, transitions are explicit
  `.action(..)`s. Update/delete return the retrieve representation (If-Match checks it).
  Admin metadata grants nothing: every admin button calls the same operation, which authorizes.
- `frontend/product/src/generated/api.ts` is generated (`make snapshots`); never edit it.
- Every caller goes through the executor (`execute`, `execute_json`, managed routes, `invoke`,
  `Registry::invoke`). Never call `Operation::run` directly; that skips the contract, policy and
  transaction.
- Contexts implement `FromInvocation` and never read the request (ADR-013). What HTTP reads is
  what `describe()` declares, so a context that needs a header must declare it there.
  `Invocation::for_actor` is for trusted callers (jobs, commands, tests); never build one from
  request data. Hand-written handlers extract `Resolved<C>`.
- After-commit work is a job (ADR-015): `jobs::enqueue` on the operation's own connection, never
  `tokio::spawn`. Handlers are at least once: idempotent, passing `job.idempotency_key()` to
  providers. Database work in a handler goes through `job.invoke` (re-authorized). Job inputs
  are stored in plain text: no credentials, tokens or link secrets in them.
- Idempotency keys (ADR-016) are opt-in per write binding (`.idempotency_key()`). The receipt is
  claimed inside the write transaction after `recheck`; never move it out or before
  authorization. Replays return the stored body only after the policy admits and rechecks.
  A key sent where none is accepted is refused, never ignored.
- Write transactions begin through `trestle_core::fence::begin` (ADR-017), never `pool.begin()`
  in the executor: the schema-fence check travels with `BEGIN` and must stay before it, so a
  refusal leaves the connection idle. A migration that changes meaning without breaking old
  SQL (units, semantics, code-only invariants) says `"fence": true`; fence migrations lock the
  fence table before any DDL and stay short.
- Reads run in `BEGIN READ ONLY` in debug builds and with `--read-only-reads` (ADR-018): a
  `Mode::Read` operation never writes; something that writes is a `Mode::Write`. Don't make it
  the release default without a cheaper mechanism (it cost ~20% read throughput). Pilot operations declare
  `const EFFECTS` (reads, writes, emits, egress) and a test requires it: update it with the
  code. A declared write may enqueue only its `emits`; never widen the check to get a job out.
- Assembly never leaks (ADR-014): no `Box::leak` on a registration path. Runtime-built apps
  register owned data (`OperationDescriptor`, `Arc<str>`, `Arc<ContractDoc>`);
  `crates/trestle/tests/assembly_memory.rs` fails if repeated assembly grows the heap.
- The executor checks every result against its output contract before a write commits (ADR-012).
  A refusal is a 500 that rolls back: fix the contract or the representation, never loosen the
  check. Its log carries field pointers and codes only, never values.
- A `ContractDoc` is the only definition of a wire contract. Never hand-write JSON Schema or
  OpenAPI; serde types bound to contracts use `deny_unknown_fields` and only map validated input.
- Published documents (`examples/pilot/published/`) change only through `make snapshots` plus a
  reviewed diff. Policy IDs and private fields never appear in them. `revisions.json` is
  append-only; retain an old form revision only if its submissions still mean the same thing.
- Metadata grants nothing: `/_meta` availability is advisory, every submit re-authorizes, and
  metadata responses are `private, no-store`.
- The browser validator is derived from published schemas; never hand-write per-form rules.
  New field types need server evaluator, schema export, TS decoder/validator and fixtures together
  (`fixtures/contracts/scalars.json` is the model, ADR-022). Decimals, dates, instants and durations
  are strings on the wire; never accept a JSON number for a decimal. Money is the one object field
  (`{amount, currency}`, ADR-027); the browser decodes only that exact shape. Their columns are read through
  `trestle_historical::sql::text_expr` (never a raw `numeric` into JSON) and bound with casts.

## Commands
- `make db`: check PostgreSQL is running and create `trestle_dev`.
- `make migrate`, `make plan`, `make seed`, `make run`.
- `make registry` (`pilot check`), `pilot inspect …`, `pilot {contracts,interactions,openapi} export`:
  offline, no database.
- `make test`: all tests. Integration tests need PostgreSQL and create/drop `trestle_test_*`.
- `make snapshots`: rewrite published snapshots after an intended contract/form/binding change.
- `pilot makemigrations [--check]`, `pilot schema show [--history]` (offline), `pilot schema verify`.
- `make frontend` (npm install), `make frontend-test`, `make web` (Vite on 5173 → API on 8000),
  `make admin` (5174), `make demo-m2` (builds `m2-release-a` in `target/e2e/`; ports 8031/4174),
  `make demo-m4` (ports 8032/4175/4176), `make demo-m5` (builds `m4-complete` into
  `target/m4-source` and `target/m4-release`; ports 18101–18150). `pilot sdk export` prints the TS
  client. `make admin` serves under `/admin/` (the admin bundle's base path).
- `make demo-accounts` (ports 18401/18420): invite by email (file backend), set password, change,
  reset, admin, lockout. `pilot account …` and `pilot mail test` are the operator commands.
- Drills and measurements: `make drill-tls` (throwaway PostgreSQL on 55433), `make drill-restore`
  (ports 18301/18302), `make load` (release builds; ports 18201/18202; ~12 minutes; do not run
  anything CPU-heavy alongside it).
- `make lint`: `cargo fmt --check` + `clippy -D warnings`. Keep both clean.

## Gotchas
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
- `tests/offline.rs` runs the binary under macOS `sandbox-exec` with network denied, plus a
  control that must fail; elsewhere it runs unsandboxed.
- Lifecycle order is topological with install order breaking ties; only "dependencies first"
  is guaranteed.
- Vite rewrites `new URL(\`…${x}\`, import.meta.url)` into an asset glob; in tests resolve paths
  with `fileURLToPath` instead. In JS, `obj["__proto__"] = …` sets the prototype; test pollution
  with `JSON.parse` input.
- Playwright: enable `page.route` to bypass the HTTP cache when asserting on response bodies.
- Chrome accepts `Secure`/`__Host-` cookies over `http://localhost` (a secure context), so the
  session flow can be exercised locally; use `localhost` (not `127.0.0.1`) as the public origin
  so `Origin` matches `TRESTLE_PUBLIC_ORIGIN` exactly.
- Load numbers on a shared development machine swing with run order and warm-up: always prewarm and alternate
  targets (`loadtest compare` does both) and compare medians, never single runs.
- React StrictMode runs `useState` initializers twice in development: keep them pure (read the
  link token there, remove it from the URL in an effect).
- Test passwords must pass the rules: not common, not numeric, and not containing the email's
  local part (`owner…` for owner@example.com fails as `too_similar`).
- A failing integration test leaks its `trestle_test_*` database; drop leftovers with
  `dropdb --force` after checking no test run is active.
