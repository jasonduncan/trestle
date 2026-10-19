# ADR-013: Operations are invoked from an `Invocation`, not an HTTP request

**Status:** Proposed (2026-10-17) · **Design doc:** §6.2, §12.1, I03, I04 · **Amends:**
ADR-003 (contexts), ADR-005 (where identity is read)

**Extended by [ADR-033](033-admin-and-application-views.md):** optional expected
operation versions have equivalent HTTP and direct-invocation refusal semantics.

## Context

The executor (`execute*`) has never needed HTTP: it takes a pool, an operation,
a context and JSON. Building that context does, though. Every context type is an
axum `FromRequestParts` extractor (pilot `TenantContext`, `ActorContext`; commons
`OwnerContext`), and `DynOperation::context_from` takes `&mut http::request::Parts`.
Each extractor does two jobs in one place:

1. **Transport work:** authenticate the caller from headers or cookies, and read
   the `Trestle-Tenant` selection.
2. **Application work:** look up the caller's membership and role.

Only the first job belongs to HTTP. A job, an operator command, a test, or a
higher layer that calls operations by ID cannot build a context without
fabricating request parts, and nothing ties `RequestContext::describe()` (what
OpenAPI promises) to what extraction actually reads.

## Decision

- **`Invocation`** (`trestle_core::invocation`) holds what the transport
  established: the authenticated actor, if any, and the tenant selection, if
  any. The selection grants nothing on its own. Fields are private, so later
  additions (deadline, idempotency key) don't break callers.
  - **HTTP:** `Invocation::from_http(parts, state, &descriptor)` authenticates
    only when the context's descriptor says `authenticated`, through the same
    `Authenticated` extractor (identity chain, actor admission, per-request
    cache). It reads `Trestle-Tenant` only when the descriptor declares that
    header. A malformed value is 400 `tenant_invalid`.
  - **Everyone else:** `Invocation::anonymous()`, or
    `Invocation::for_actor(id).with_tenant(t)`. `Invocation::for_actor` is a trusted
    constructor, for jobs that recorded their actor, operator commands and
    tests. HTTP never uses it.
- **`FromInvocation<S>`** replaces `FromRequestParts` for operation contexts. It
  is the application's lookup (membership, role, owners), and every caller runs
  the same code. `invocation.actor()?` is 401 `credentials_missing` when there
  is no actor.
- **`DynOperation::context_from(&mut Parts, &S)`** becomes
  `resolve_context(&Invocation, &S)`.
- **Direct invocation.** `operation::invoke` and `invoke_dyn` resolve the
  context and run the executor. `Registry::invoke(state, id, &invocation,
  &input)` does the same by stable operation ID and returns JSON, and
  `TestApp::invoke` exposes it to project tests. Managed routes, `/_meta`
  availability, admin checks and hand-written handlers (the `Resolved<C>`
  extractor) all build an `Invocation` and resolve through the same trait.
- Admission (rate limits) stays an HTTP perimeter concern. Direct invocations
  are not charged.
- `TENANT_HEADER` moves to `trestle_core::invocation`. The facade keeps its
  re-export.

## Consequences

- **Breaking:**
  - context types implement `FromInvocation<S>`, not `FromRequestParts<S>`;
  - `DynOperation` implementors rename and re-sign `context_from`;
  - states used for managed routes need `Identity: FromRef<S>`.

  In this repo that means the pilot's two contexts, the intent runtime and the
  registry tests. Downstream, commons' `OwnerContext` changes when it moves its
  pin; smart-folder registers no operations.
- A context's descriptor is now behavior. A context that reads a header it
  doesn't declare cannot see it, and an undeclared requirement cannot hide from
  OpenAPI.
- Jobs (U4) and higher layers get one authorized entry point by operation ID,
  without HTTP.

## Evidence

- `examples/pilot/tests/invocation.rs`:
  - the same `lessons.create` by a teacher, a viewer, another school's
    teacher, a caller with no tenant and a caller with no credentials ends the
    same way over HTTP and through `Registry::invoke` (201 /
    `permission_denied` / `tenant_not_permitted` / `tenant_required` /
    `credentials_missing`);
  - only the two permitted creates are written, both owned by the
    invocation's actor;
  - a direct read of another tenant's lesson is 404, as over HTTP;
  - `schools.mine`, which declares no tenant, ignores a malformed
    `Trestle-Tenant`, which `lessons.list` refuses;
  - an unknown operation ID is 404.
- `crates/trestle/tests/intent.rs`: intent-file operations invoked by ID run
  their hooks and match their HTTP routes on member, viewer and other-tenant
  callers.
- `invocation.rs` unit tests: declared credentials and tenant are read, and
  build the same value as `for_actor(..).with_tenant(..)`; undeclared ones,
  including bad credentials and a malformed tenant, are never read.
- The existing workspace suite passes unchanged.
