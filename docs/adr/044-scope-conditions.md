# ADR-044: Conditions on a scope's `through` row

**Status:** Proposed (2026-10-22; design agreed by the project owner) ·
**Design doc:** §3.4, §6.2 · **Amends:** the intent format, ADR-040 ·
**Depends on:** ADR-025, ADR-036, ADR-040

## Context

A `through` scope (ADR-040) admits a row when *some* row of the `through`
model links it to the actor:

```toml
team = { field = "team", through = "crm.team_member", key = "team", actor = "actor" }
```

Membership tables rarely mean "any row grants reach". They carry state:
`active = false` for someone who left the team but whose history is kept,
or a `role` that says who manages the team and who only belongs to it.
Under ADR-040, an inactive membership or a plain member's row grants the
same reach as an active manager's. The only ways around that were deleting
the row (losing the record) or a `before_write` hook, which can refuse a
write but can't filter a list or hide a row from `get`.

Generated applications (Proseware is one) need "active managers of the
team" as data in the intent file, like the rest of a scope.

## Decision

### `where` on a `through` scope

```toml
[[models]]
id = "crm.team_member"
fields = [
  { id = "team", type = "ref", to = "crm.team" },
  { id = "actor", type = "uuid" },
  { id = "active", type = "bool", default = true },
  { id = "role", type = "enum", values = ["member", "manager", "lead"], default = "member" },
]

[models.scopes]   # of crm.opportunity
team = { field = "team", through = "crm.team_member", key = "team", actor = "actor",
         where = { active = true, role = ["manager", "lead"] } }
```

- Each entry names a field of the `through` model and a constant it must
  equal, or a list of constants it must be one of. Every entry must hold
  (AND), and a list is membership (`IN`).
- The match becomes `EXISTS (SELECT 1 FROM <through> x WHERE x.<tenant> =
  t.<tenant> AND x.<key> = t.<field> AND x.<actor> = $actor AND x.active =
  true AND x.role IN ('manager', 'lead'))`. A null field meets no
  condition.
- `IntentDoc` carries it as `ScopeDoc.conditions` (key `where`, a map of
  field → `toml::Value`, scalar or array). `Through.conditions: Vec<(field,
  constants)>` holds the resolved form, in field order, with constants as
  JSON like an action's `when` (ADR-025).

Why this shape:

- **Constants only, like `when` and `check` (ADR-025, ADR-036).** The
  conditions are part of the file's authority rules and are reviewed with
  it. A comparison with the actor's other data, or another table, is a
  multi-hop scope (below).
- **Equality and membership only.** That is what "active", "role" and
  "kind" fields need. Ordering comparisons on a membership row (`level >=
  3`) have no use case yet; `check`'s operators can be added under the
  same key later without changing what these entries mean.
- **`where`, not a new scope kind.** It narrows the existing `through`
  match, so every rule and path of ADR-040 applies unchanged.

### Parse rules

Each a parse problem with the scope's line:

- `where` on a direct scope (no `through`): the direct form reads the
  actor from the row itself, and there is no other row to condition;
- a name that is not a field of the `through` model;
- a field that is not `bool`, `int`, `text` or `enum` (IDs, money,
  decimals, dates, instants and durations are left out);
- a constant that doesn't fit the field, by the same `fits` rule as an
  action's `set` and `when` (an enum constant must be one of its values),
  an empty list, or a string with a NUL character.

### Where the conditions apply

Everywhere the scope does, because the predicate is rendered in one place
(`RowFilter::sql`):

| Path | Effect of `where` |
|---|---|
| list, `filter`, `q`, cursor pages | only rows reached through a matching `through` row are selected |
| get, and the fetch of update, delete and action | a row reached only through non-matching `through` rows is 404 `not_found`, before `If-Match` |
| the row as written (create, update, action) | outside every granted scope with its conditions is 403 `out_of_scope`, and the write rolls back |
| idempotent replay (`recheck_replay`) | the row now, or a delete's stored row, is rechecked with the conditions; a miss is the 404 of a fresh request |

### Concurrency

The locked fetch and the after-write check already lock the `through` row
`FOR SHARE` (ADR-040). The same lock covers updates:

- An `UPDATE` of the `through` row (e.g. `active = false`) takes a
  row lock that conflicts with `FOR SHARE`. A write that reaches the
  membership while such an update is uncommitted waits. After the update
  commits, PostgreSQL rechecks the subquery's conditions on the committed
  row version (READ COMMITTED), so the condition no longer holds and the
  write is a 404, exactly like a deleted membership.
- An update that starts after the write locked the row waits for the
  write's commit, as a deletion does.

The constants are the file's, never request data. They are rendered as SQL
literals with `trestle_historical::sql::literal` (the escaping migrations
use) rather than bound, so every call site of the predicate keeps its
parameter numbering. A constant that couldn't be rendered becomes `NULL`,
which no row meets; the parse rules make that unreachable.

### Multi-hop scopes stay out

`where` conditions the one `through` row. Scopes that go through a second
model (a team through its department, a manager through the actor's
`crm.member` row) remain future work (ADR-040, open questions): they need
a chain of joins, a lock on every link, and a rule for which links a
`where` names.

## Evidence

`crates/trestle/tests/intent_scopes.rs` (PostgreSQL), on ADR-040's CRM
fixture with `active` and `role` added to `crm.team_member` and the `team`
scope conditioned `where = { active = true, role = ["manager", "lead"] }`:

- `scope_conditions_are_checked_when_the_file_is_read`: the resolved
  `Through.conditions` are `active = [true]` and `role = ["manager",
  "lead"]`. A file with `active = "yes"`, `role = ["manager", "boss"]`, an
  unknown field, a `uuid` field and `where` on a direct scope reports
  exactly those five problems.
- `a_scope_condition_admits_only_matching_through_rows`: while the
  manager's team-A membership has role `member`, the manager lists nothing
  and Bravo is 404. As `manager`, and as `lead`, the manager lists Alpha
  and Bravo, and creates Echo with an `Idempotency-Key`. With the same
  membership `active = false`: the list is empty, Bravo's get is 404, the
  key's replay is 404 without the stored row and not marked as a replay,
  and a new create in team A is 403 `out_of_scope`. Salespeople's `own`
  scope, without conditions, is unaffected.
- `a_scope_condition_is_rechecked_in_the_transaction`: a test transaction
  sets the manager's membership `active = false` without committing. The
  manager's `win` of Bravo waits on it (a lock wait in
  `pg_stat_activity`), and after the commit it is 404 and Bravo stays
  open. Deterministic: the action can't finish before the commit, and the
  test fails if it does.
- `intent_doc.rs`: `every_key.toml` has a `where` with a scalar and a list,
  and the document round-trips and assembles like its file.

Mutation checks, each caught:

- **Conditions left out of the locked predicate** (lists still
  filtered): the recheck test's `win` is 200, and the inactive manager's
  replay returns the stored 201.
- **`FOR SHARE` dropped:** "the action did not wait for the team
  membership update".

## Consequences

- **"Active managers of the team" is one key** on the scope, enforced on
  every generated path and rechecked in the write's transaction.
- **Deactivating a membership revokes reach immediately,** including for
  writes in flight and idempotent replays, without deleting the row.
- **Index for it** on large tables: the `EXISTS` now also filters on the
  condition fields; a partial or composite index on the `through` model's
  `key`, `actor` and condition fields keeps it an index lookup.
- **Breaking for struct literals:** `ScopeDoc` has a `conditions` field
  (`..Default::default()` covers it) and `Through` a `conditions` member.
  Files that parsed before mean the same: a scope without `where` admits
  any matching row, as in ADR-040.

### Open questions

- **Ordering operators** (`check`'s `ge`, `lt`, ...) in `where`, and
  conditions on instants (`ends_at > now()`, an expiring membership). Both
  fit the same key when a project needs them.
- **Multi-hop scopes,** above.
