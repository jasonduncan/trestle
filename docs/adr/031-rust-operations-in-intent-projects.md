# ADR-031: Rust operations in intent projects

**Status:** Proposed (2026-10-18; design agreed by the project owner) ·
**Design doc:** §3.4, §6.2 · **Amends:** the intent API, ADR-029 ·
**Depends on:** ADR-013, ADR-014, ADR-030

## Context

An intent project extends `trestle.toml` with Rust in only two ways:

- a named **handler**, which receives the intent's data handle (`Ctx`) but
  must return one row of the endpoint's model;
- a **hook** on a CRUD endpoint.

An endpoint whose output isn't a row, such as a report, an aggregate or a
multi-step command, must be a hand-written `DynOperation`. That operation
gets nothing from the intent:

- the role check (`IntentPolicy`) and the scoped data handle are private;
- the linked intent isn't reachable from an installed app.

So the operation has to repeat the tenancy, the membership recheck and the
CRUD SQL, which are exactly what the intent exists to own.

Separately, nothing tested a runtime-built operation in three cases:

- under the schema fence;
- with a stale context rechecked in its transaction;
- behind `ResourceOp` and `admin_dyn` when the operation was hand-written.

## Decision

- **`trestle::intent::Permission`** is a permission from `[roles]`, checked
  the way generated operations check it. `Permission::new(&resolved, name)`
  accepts `authenticated` or a permission some role grants. It provides:
  - `id()`: the same policy ID as generated operations,
    `intent.permission.<name>`;
  - `context()`;
  - `resolve(invocation, state)`: the actor, the tenant and the role there.
    A tenant without membership is refused like an unknown one;
  - `admit(ctx)`;
  - `recheck(conn, ctx)`: on the membership as it is now, locked
    `FOR SHARE`.

  It implements `DynPolicy`, replacing the private `IntentPolicy` behind
  `[[admin]]`. Generated operations use it too, so there is one check.
- **`Ctx::run(&resolved, conn, &ctx, f)`** gives `f` the intent's data
  handle (`get`, `list`, `create`, `update`, `delete`) on a `DynOperation`'s
  own connection, scoped to the resolved caller:
  - it always hands the connection back, in the shape `DynOperation::run`
    returns;
  - the handle writes only on `DynConn::Write`, the executor's write
    transaction;
  - generated operations run through it too.
- **`Project::intent_app(|resolved| app)`** installs an app built from the
  project's linked intent, after linking. The app shares the same
  `Arc<Resolved>`. Assembly fails if the project has no intent.
- **Nothing is skipped.** These operations run through the executor like
  any other: contract validation, admission, the fenced `BEGIN`, recheck,
  declared effects, then the output check before commit. Showing an admin
  view still grants nothing.

## Consequences

- An intent project can carry Rust operations with any contract and keep a
  single definition of tenancy, roles and data access.
- `tests/intent_permission.rs` covers:
  - HTTP and `Registry::invoke` decisions;
  - a stale context, demoted and removed;
  - a failed operation, a refused output and a read that tries to write,
    each leaving nothing;
  - a hand-written resource with `admin_dyn`.

  `trestle-core/tests/fence.rs` now fences a runtime-built write as well.
- `Ctx` construction stays private. The only ways to get one are a handler,
  a hook or `Ctx::run`, so a handle is always tied to an executor
  connection and a resolved caller.
