# ADR-038: Idempotent intent endpoints

**Status:** Proposed (2026-10-21; design agreed by the project owner) ·
**Design doc:** §6.4 · **Amends:** the intent format, ADR-016 ·
**Depends on:** ADR-016, ADR-025, ADR-029, ADR-031

## Context

ADR-016 built `Idempotency-Key` receipts: claimed in the write
transaction after `recheck`, replayed only after the policy admits and
rechecks, 409 for the same key with another input. A Rust binding opts in
with `Http::idempotency_key()`, and the project installs
`trestle_core::idempotency::IdempotencyApp` by hand.

Nothing in `trestle.toml` could opt in. A client that lost the answer to a
generated `POST /api/v1/deals` had two bad choices: retry and maybe create
the deal twice, or not retry and maybe lose it. Proseware, which generates
`trestle.toml` files, needs safe retries without writing Rust, and so does
any project whose clients sit on unreliable networks.

## Decision

- **One key per endpoint:** `idempotent = true` (default `false`).
  - On a **CRUD endpoint** it covers the endpoint's writes: create, update
    and delete. List and get never take a key.
  - On an **action** it covers the action.
  - On a **handler endpoint** it covers the binding when its method is
    not `GET` (the operation is then a `Mode::Write`).
  - **Problems when the file is read:** a CRUD endpoint that lists only
    `list` and `get`, and a `GET` handler.
  - **Not per verb.** A model has one CRUD endpoint, and a client that
    retries creates retries updates for the same reason. A finer form
    (`idempotent = ["create"]`) can be added later without breaking
    `true`.
- **Exactly ADR-016's binding.** The generated binding calls
  `Http::idempotency_key()`, so everything ADR-016 says holds unchanged:
  - the receipt is claimed in the write transaction after `recheck`, and a
    replay happens only after the policy admits and rechecks;
  - a replay is the stored body with the binding's status, `Location`
    and `ETag`, plus `Idempotency-Replayed: true`;
  - another input under the same key is 409 `idempotency_key_reused`;
  - a malformed key is 400 `idempotency_key_invalid`;
  - `Registry::invoke` honours a key for these operations;
  - operation jobs that target them are keyed by the job ID.

  Version checks are part of the input: a replayed update or action with
  the same `If-Match` returns the first answer, not 412.
- **Refused elsewhere.** Every endpoint without `idempotent = true` keeps
  refusing a key with 400 `idempotency_unsupported`, over HTTP and through
  `Registry::invoke`. A key is never ignored.
- **Resources.** CRUD endpoints are resources (ADR-029), whose verbs build
  their own bindings. `ResourceRegistration::idempotency_key()` keys the
  conventional write verbs registered after it: create, update, delete and
  `action`. `action_at` keeps the binding it is given, so an action on an
  idempotent model's resource is keyed only by its own `idempotent`. The
  generated resource calls it first. Rust resources can use it too.
- **The receipt table comes with the file.** When some endpoint is
  idempotent (`Intent::idempotent`), the project installs `IdempotencyApp`
  with the generated apps, as ADR-034 does for the identity app:
  - `makemigrations` plans its `trestle_idempotency` table like any app's,
    and `serve` requires its committed migrations applied (with the intent
    apps' own);
  - a project that installed `IdempotencyApp` itself keeps its own, so it
    is never installed twice;
  - a project without an idempotent endpoint gets nothing new: no app, no
    table, no migration;
  - an intent app named `idempotency` is refused when the project would
    install the receipt app, rather than colliding with it at assembly.
- **Documents.** OpenAPI lists the optional header on the keyed bindings,
  and interaction descriptors of forms over them say
  `{"mode": "key", "header": "Idempotency-Key", "retentionSeconds": 86400}`,
  as ADR-016 does for any keyed binding. Every other document is unchanged.
- **`IntentDoc`:** `EndpointDoc.idempotent`. `EndpointSpec::Crud`, `Custom`
  and `Action` each have `idempotent`; `EndpointSpec::idempotent()` reads
  it from any of them.

## Evidence

`crates/trestle/tests/intent_actions.rs` (PostgreSQL), on the CRM intent
with an idempotent CRUD endpoint (get, create, update), an idempotent
`crm.win` and a plain `crm.reopen`:

- `idempotent_endpoints_are_writes_and_bring_the_receipt_table`:
  - `idempotent = true` on a get-only CRUD endpoint and on a `GET` handler
    are parse problems;
  - exactly `crm.deal.create`, `crm.deal.update` and `crm.win` accept a
    key, and OpenAPI documents the header three times;
  - the receipt app is installed once, also when the project installed it
    itself, and not at all for the plain CRM file (no receipt model);
  - `makemigrations` writes `idempotency/0001_initial.sql` with the
    intent's migrations, and `--check` is then clean; for the plain file
    it writes no `idempotency` directory.
- `idempotent_endpoints_replay_after_reauthorizing`:
  - create: the repeat is 201, marked replayed, with the same bytes,
    `Location` and `ETag`, and one row; another body under the key is 409
    `idempotency_key_reused`, still one row;
  - `crm.win`: the repeat returns the first answer's bytes and `ETag`, and
    the row stays at version 2; another body is 409;
  - update: the repeat with the same `If-Match` replays;
  - a key on `crm.reopen` and on get is 400 `idempotency_unsupported`,
    and the reopen did not run;
  - `Registry::invoke` with the HTTP create's key returns the same row,
    and with a key for `crm.reopen` is `idempotency_unsupported`;
  - after the member is demoted to viewer, the create key's repeat is 403
    `permission_denied`, not a replay. As in ADR-016, this is refused by
    `admit` before the transaction, so it does not isolate `recheck`; the
    keyed path rechecks before claiming like every write.
- `intent_doc.rs`: `every_key.toml` sets `idempotent` on a CRUD endpoint,
  a handler and an action; the document round-trips and assembles like its
  file.

Mutation checks, each caught: not installing the receipt app (assembly
refuses the keyed operations), and not keying the CRUD resource (create
and update refuse keys).

## Consequences

- **Safe retries without Rust** for generated writes, with the semantics
  and costs of ADR-016: a keyed write waits for a concurrent duplicate, and
  receipts keep response bodies for 24 h (`idempotency::purge_expired`).
- **Breaking for exhaustive patterns:** the three `EndpointSpec` variants
  have a new member, and `EndpointDoc` struct literals need
  `..Default::default()`.
- **A new installed app** when the file opts in. Projects turning it on
  run `makemigrations` and `migrate` before `serve`.
- **Clients must send the key.** The React renderer and the admin shell do
  not send keys yet, so their writes behave as before.
