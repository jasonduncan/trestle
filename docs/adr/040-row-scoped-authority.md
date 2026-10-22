# ADR-040: Row-scoped authority

**Status:** Proposed (2026-10-21; design agreed by the project owner) ·
**Design doc:** §3.4, §6.2 · **Amends:** the intent format, the intent API,
ADR-016, ADR-031, ADR-038 · **Depends on:** ADR-024, ADR-025, ADR-029,
ADR-030, ADR-033, ADR-039

**Extended by [ADR-044](044-scope-conditions.md):** a `through` scope's
`where = { active = true, role = ["manager", "lead"] }` admits only
`through` rows whose fields equal those constants, on every path below,
and an update that breaks a condition is rechecked like a deletion.

## Context

`[roles]` gives each tenant membership one role, and a role holds a set of
permissions. A permission admits its operations on every row of the
tenant. Many applications need narrower authority than that:

- salespeople edit the opportunities they own;
- managers manage their team's records;
- everyone else with the permission sees all of them.

Django answers this with object permissions (django-guardian, DRF's
`has_object_permission` and `get_queryset` filtering). An intent project
could only do it in Rust, and only partly. A hook can refuse a write. It
can't filter a list, though, and a `get` would still show that a hidden
row exists. Proseware, which generates `trestle.toml` files, needs it in
the intent as data.

## Decision

### Row scopes on a model

A model may declare named **row scopes**. Each one ties a row to the
acting actor:

```toml
[[models]]
id = "crm.opportunity"
fields = [
  { id = "owner", type = "ref", to = "crm.member" },
  { id = "team",  type = "ref", to = "crm.team" },
  # ...
]

[models.scopes]
own  = { field = "owner", through = "crm.member", actor = "actor" }
team = { field = "team", through = "crm.team_member", key = "team", actor = "actor" }
mine = { field = "created_by" }
```

- **`own`:** some `crm.member` row whose `id` equals the row's `owner` has
  the current actor in its `actor` field. `key` defaults to `id`.
- **`team`:** some `crm.team_member` row whose `team` equals the row's
  `team` has the actor.
- **Direct form, `mine`:** no `through`. The row's own `uuid` field holds
  the actor.

The `through` model is an intent model of the same tenant, and the match
is tenant-scoped in SQL. The rules, each a parse problem with the scope's
line:

- `field` is a field of the model;
- without `through`, it is a `uuid` field, and `key` and `actor` are not
  given;
- with `through`:
  - the model is in the file;
  - `actor` is a `uuid` field of it;
  - `key` is `id` or one of its fields;
  - `key` and `field` compare as the same type, the same rule as `exists`
    (ADR-036): a `ref` points to the model `key` names, or both are
    `uuid`;
- scopes need `[tenancy]`.

`IntentDoc` carries this as `ModelDoc.scopes:
BTreeMap<String, ScopeDoc>` (`field`, `through`, `key`, `actor`), and
`ModelSpec.scopes: BTreeMap<String, ScopeSpec>` holds the resolved form,
with the `through` model's table. The format stays data only.

### Scoped grants

In `[roles]`, a grant is a permission, or `"<permission>@<scope>"` for the
rows of that scope only:

```toml
[roles]
salesperson = ["crm.read", "crm.sell@own"]
manager     = ["crm.read", "crm.sell@team"]
director    = ["crm.read", "crm.sell"]
```

Why this syntax:

- **Grants stay strings.** `IntentDoc.roles` is unchanged
  (`Vec<String>`), so existing files and documents mean what they did.
  A table form (`{ permission = "crm.sell", scopes = ["own"] }`) would make
  a role a mixed array and every reader handle two shapes.
- **One entry grants one thing.** The entry is a line in a diff, and
  several scopes are several entries (`"crm.sell@own"`, `"crm.sell@team"`).
- **`@` was never part of a permission name in practice,** and scope names
  are identifiers.

The rules for grants:

- An unscoped grant of P admits every row of the tenant, as before, and
  absorbs scoped grants of P in the same role.
- `P@s` is a parse problem when no model declares a scope `s`, and for
  `authenticated`, which every signed-in actor holds anyway.
- Scope names are shared across models: `P@own` admits, on each model
  that declares `own`, the rows of that model's `own`.

`Intent.roles` now holds each role's unscoped permissions only. Every role
is still a key, so a role whose grants are all scoped has an empty set
there. The new `Intent.scoped` holds role → permission → scopes for the
rest. Code that reads `roles` as "holds P everywhere" stays correct.

### What a grant admits

`Permission` knows what it guards:

| Guarded | Unscoped grant | Scoped grant |
|---|---|---|
| A generated operation on a model's rows: CRUD and actions | every row | the rows of the granted scopes that this model declares; none declared is a 403 `permission_denied` |
| Showing an admin or application view (ADR-029, ADR-033) | shown | shown, because a view grants nothing |
| A handler endpoint, or `Permission::new` in a Rust operation (ADR-031) | admitted | 403 `permission_denied` |

A handler or a Rust operation gets a `Ctx`, which sees the whole tenant.
A scoped grant must not admit code written for "holds P", or it would
widen silently. Rust that wants row scoping has no API for it yet (see
open questions).

On a model that declares none of the granted scopes, a scoped-only grant
is a 403 rather than an empty list. The role simply doesn't hold the
permission there, and an empty page would hide a mistake in the file.

### Enforcement, per path

The scope predicate is `(scope OR scope ...)` over the granted scopes the
model declares. It is applied in SQL right after the tenant condition:
`t.<field> = $actor`, or `EXISTS (SELECT 1 FROM <through> x WHERE
x.<tenant> = t.<tenant> AND x.<key> = t.<field> AND x.<actor> = $actor)`.

| Path | With a scoped grant |
|---|---|
| list, `filter`, `q` search, cursor pages | Only rows inside the scopes are selected. Pages, `next_cursor` and filters never reveal another row; the cursor is already bound to the actor and tenant. |
| get | A row outside the scopes is 404 `not_found`, exactly like another tenant's. |
| update, delete, action | The row is found and locked `FOR UPDATE` through the predicate, with the `through` row locked `FOR SHARE`. Outside the scopes, it is 404 before `If-Match` is compared, so a stale version reveals nothing. |
| create, update, action: the row as written | It must satisfy a granted scope, with the `through` row locked `FOR SHARE`. This is checked on the row as it will be stored (endpoint hooks, `before_write` and declared defaults included) before the statement runs, and again on the written row. Otherwise the result is 403 `out_of_scope` and the transaction rolls back. |
| `Registry::invoke`, jobs, admin and view buttons | They call the same operations through the executor, so the same decisions apply. |

**Create outside the scopes is a 403, not a 422.** The body is well formed.
The caller lacks authority for that row, and the same answer serves create,
update and actions. A body-supplied `owner` that isn't the caller's member
row is refused for an `@own` grant. A hook may still fill `owner` from the
actor, because the check sees the row as written.

**Reassignment.** The row must be inside a granted scope before the write
(else 404) and after it (else 403):

- an `@own` salesperson can't hand an opportunity to someone else;
- an `@team` manager can reassign the owner within the team, since the row
  stays in `team`, but can't move it to another team;
- an unscoped grant can do either.

This is how "managers reassign within their team" is expressed, without a
special rule.

### Recheck inside the transaction

Writes already recheck membership `FOR SHARE` (ADR-031).
Scopes add two checks:

- **The scope's `through` row is locked `FOR SHARE`,** in the locked
  fetch and in the after-write check, until commit:
  - a concurrent removal of the team membership waits for the write;
  - a write that reaches a membership being removed waits, then sees it
    gone, and is a 404.
  The direct form needs no lock, because it reads the locked row itself.
- **`Permission::recheck` compares the role now with the role resolved
  outside the transaction.** If the role changed, the new grant must admit
  every row the old one did: unscoped covers anything, and scoped covers
  a subset of its scopes. Otherwise the result is 403
  `permission_denied`. A director demoted to salesperson between
  resolution and the write's transaction can't write with the director's
  reach.

### Refs, guards and trusted code

- **Ref writes (ADR-024) and `exists` guards (ADR-036) check tenancy, not
  scopes.** A ref names a row the record depends on, and the referenced
  row may be one the caller can't read, such as a manager as owner. This
  is the same reasoning as ADR-024: an existence check on an ID the
  caller already holds reveals nothing more. A ref error says only
  whether that ID exists in the tenant (`not_found`, like a missing one),
  and never returns a hidden row's fields. Lists, gets and their counts
  never include hidden rows, so scoped reads give no way to enumerate
  IDs. The one exception is a tenant-wide unique set, below: a write
  inside the scopes learns whether any row holds its value.
- **Unique sets are tenant-wide.** A write that would leave the scopes is
  refused before it reaches a unique index, so a hidden row's values never
  change its answer (fixed in 0.8.1: the check used to come after the
  write, and a unique violation answered first). A write inside the
  scopes still gets the 422 `already_exists` when any row of the tenant,
  hidden or not, holds the value: that is what a tenant-wide unique set
  means. A model whose unique values must stay private to its scopes
  needs the field of each scope a grant uses in the set: with `own` and
  `team`, a set of `team` and `title` still lets an `@own` caller learn
  whether a teammate's row holds a title. Loading a file logs a warning
  for each unique set that leaves out such a field, naming the scopes
  (`Intent::warnings`). It isn't refused, since a tenant-wide set can be
  what the project wants.
- **`Ctx` keeps full tenant access.** Handlers, endpoint hooks, model
  write hooks (ADR-039) and `Ctx::run` are trusted code. Their `get`,
  `list` and writes are tenant-scoped, not row-scoped. A hook can apply
  richer rules with `Ctx` queries, such as approval authority or "only the
  manager may set `stage = won`", and refuse with a problem.
- **Admin and application views grant nothing.** Their data reads are the
  scoped operations themselves.

### Idempotent replays

ADR-016 replays a stored answer only after the policy admits and
rechecks. With row scopes, the recheck includes the row, so a replay never
returns a record the caller can no longer reach:

- **`DynOperation::recheck_replay(conn, ctx, output)`** is new in
  `trestle-core`. It runs inside the write transaction, after `recheck`
  and the receipt claim, before a replay returns its stored body. It
  defaults to admitting, so existing operations are unchanged. A refusal
  is the operation's response, the stored body is never sent, and the
  transaction rolls back.
- **Generated operations under a scoped grant** implement it:
  - a create, update or action finds the row as it is now, through the
    scopes, with the `through` row locked `FOR SHARE`, as a fresh write
    would;
  - a delete, whose row is gone, evaluates the scope predicate on the
    stored row's scope fields, with the same lock.

  If the row is missing or outside the scopes, the replay is the 404
  `not_found` of a fresh request. A manager removed from the team who
  replays a key gets that 404, not the stored row. Unscoped grants replay
  exactly as before.

### Declared effects

Operations on a model with `through` scopes declare `reads` of those
models (ADR-018), because a scoped grant's predicate reads them.

## Evidence

`crates/trestle/tests/intent_scopes.rs` (PostgreSQL) uses a CRM fixture:

- `crm.member`, with an `actor` uuid;
- `crm.team` and `crm.team_member`;
- `crm.opportunity`, with `owner` and `team` refs and scopes `own` and
  `team`;
- salespeople S1 and S2 in team A and S3 in team B (`crm.sell@own`), a
  manager of team A (`crm.sell@team`), a director (`crm.sell`), and an
  admin without a sales grant.

The tests:

- `scopes_and_scoped_grants_are_checked_when_the_file_is_read` covers a
  grant of an undeclared scope, `authenticated@own`, a malformed grant, a
  non-uuid `actor`, mismatched `key` and `field` types, a direct scope on
  a non-uuid field, an unknown `through` model, a missing `actor` and an
  unknown field. These are exactly nine problems, with the lines of the
  scope or grant.
- `scoped_operations_declare_the_models_their_scopes_read` checks the
  list's and the action's `reads`.
- `a_scoped_grant_admits_only_the_rows_of_its_scopes`:
  - **Lists:** S1 lists only Alpha, S3 only Charlie, the manager Alpha
    and Bravo, and the director all three. Filters and `q` stay inside
    the scopes. The admin gets a 403.
  - **Hidden rows:** S1's get, PATCH, DELETE and `win` of S2's Bravo are
    each 404 `not_found`, and so is `Registry::invoke` of the get.
  - **Create:** S1 creating with S2's member as owner is 403
    `out_of_scope` and nothing is created. With their own member row it
    is 201.
  - **Paging:** S1's pages hold only their rows, and the last cursor
    ends.
  - **Reassigning, S1:** S1's PATCH of `owner` to S2 is 403 and the owner
    is unchanged, while a rename is 200.
  - **Handler endpoint:** S1 gets 403 `permission_denied` and the
    director 200.
  - **Manager:** the manager wins Bravo (200) but not team B's Charlie
    (404). Reassigning Alpha to S2 is 200; moving it to team B is 403. S1
    then no longer lists Alpha.
- `a_scoped_write_rechecks_its_scope_in_the_transaction`: a test
  transaction deletes the manager's team membership without committing.
  The manager's `win` of Bravo waits on it, which shows in
  `pg_stat_activity` as a lock wait. After the commit, it is 404 and
  Bravo stays open. This is deterministic: the action can't finish before
  the commit, and the test fails if it does.
- `a_replay_rechecks_the_rows_scope`: the manager creates an
  opportunity in team A and renames Bravo, each with an
  `Idempotency-Key`. A replay then returns the stored 201 with
  `Idempotency-Replayed`. After the manager's team membership is
  removed, replaying either key is 404 `not_found`, not marked as a
  replay, and without the row's fields. The test is deterministic: the
  removal commits before the replays.
- `intent_doc.rs`: `every_key.toml` declares a direct scope, a `through`
  scope with `key`, and scoped grants. The document round-trips and
  assembles like its file.

Mutation checks, both caught:

- **`FOR SHARE` dropped from the scope predicate:** the recheck test fails
  ("the action did not wait for the team membership").
- **`recheck_replay` not called:** the removed manager's replay returns
  the stored 201.

## Consequences

- **Row-level permissions are data.** "Salespeople edit what they own,
  managers manage their team's" is two scopes and two grants.
- **Scoped grants never reach Rust code with tenant-wide access.** A
  project that wants a salesperson to use a handler endpoint grants an
  unscoped permission for it.
- **A list with scopes runs an `EXISTS` per row candidate.** Index the
  `through` model's `key` and `actor` fields for large tables. Refs are
  already indexed (ADR-024); a plain `uuid` actor field is not.
- **A scoped replay costs one more locked lookup,** the same one a fresh
  write makes.
- **Breaking for struct literals and readers:**
  - `Intent` has a `scoped` member, and `Intent.roles` no longer lists
    scoped grants;
  - `ModelSpec` has `scopes`, and `ModelDoc` a `scopes` field
    (`..Default::default()` covers it);
  - a permission string containing `@` is now a scoped grant;
  - `Permission::new` admits only unscoped grants. That is the same
    decision as before for files without scopes.

### Open questions

- **Rust operations that honour scopes:** a `Ctx` narrowed to a
  permission's rows, or `Permission::rows` made public. They are left out
  until a project needs them.
- **Multi-hop scopes** (`team` through a department) and field-level
  authority (only managers may change `owner`). Use a hook for now.
