# ADR-003: Operation execution and scoped persistence

**Status:** Accepted (M1, 2026-09-26) · **Design doc:** §6, §12, I03, I04, I12, I13 ·
**Amended by:** ADR-012 (output check before commit), ADR-013 (contexts resolve from an
`Invocation`), ADR-014 (owned `DynOperation` descriptors), ADR-016 (idempotency keys),
ADR-018 (declared effects; reads run read-only)

## Context

In M0, `create_lesson` did everything inline: role check, begin transaction,
membership recheck under `FOR SHARE`, insert, audit, classified commit. M1
requires extracting an executor from that path without changing behavior,
so that API, forms, admin and jobs share one authorized entry point (I04)
while ordinary Axum handlers remain possible (I13).

## Decision

- **`Operation` trait** (`trestle_core::operation`) with associated
  `Context`, `Input`, `Output` and `Policy` types, a static `OperationInfo`
  (stable ID, version, `Mode::Read | Write`, audience, summary), and
  `run(&mut PgConnection, &Context, Input)`.
- **Policy is required by the type system.** `Policy::admit` runs before any
  transaction. `Policy::recheck` runs inside the write transaction before
  `run`. There is no default. A `compile_fail` doctest proves an operation
  without a policy does not compile (E0046). A public operation must name a
  policy that admits everyone.
- **The executor owns the transaction.** For `Write` it runs begin → recheck
  → run → `db::commit`, where a COMMIT without a server reply becomes
  `outcome_unknown`. For `Read` it runs on a plain pooled connection.
  Repositories still take an explicit `&mut PgConnection`.
- **No way around the contract.** `execute` (typed input is serialized and
  revalidated), `execute_json` and managed routes all validate first.
  `execute_validated` accepts only `Validated<T>`, which only `validate` can
  construct.
- **Managed routes** are generated from `Http::get/post` bindings. They
  extract the context before reading the body, map a contract failure on a
  path parameter to 404 (as M0 did for malformed IDs), serialize the typed
  output in declaration order, and fill `Location` from output fields.
- **Tenant context stays product code.** `TenantContext` implements
  `FromRequestParts` (authentication, header selection, membership lookup)
  and `RequestContext::describe` (for OpenAPI/inspection). The framework does
  not assume a user or tenant table.

## Evidence

- `examples/pilot/tests/lessons_api.rs`: the 13 M0 API tests (cross-tenant 404,
  header-is-not-authority 403, viewer 403, revocation, malformed/oversized
  bodies, unknown fields, the unique-constraint field error, audit failure
  rolling back the lesson, readiness) pass. Only two call sites changed.
- `operation_rechecks_membership_inside_its_transaction` drives the executor
  directly with a stale context and is denied by `recheck`.
- `examples/pilot/tests/conformance.rs` shows a hand-written handler calling
  `execute_json` behaves identically to the managed route.

## Consequences

- An operation cannot open its own transaction or skip recheck. Multi-step
  workflows need one operation, not chained endpoint calls (§6.3).
- Audit is still written by each operation inside the transaction. An
  executor-level audit hook waits until a second writing operation shows
  the pattern.
- Idempotency keys, `expected_version` and ETags (§6.4) are not implemented.
  The interaction descriptors say `idempotency: none` and
  `concurrency: not_applicable` accordingly.

## Alternatives rejected

- **Closure-based operation registration:** it has less type safety, and the
  policy would become an optional argument.
- **Policy as an optional registry binding checked at assembly:** it is
  weaker than a compile error and invites "temporarily unprotected"
  operations.
