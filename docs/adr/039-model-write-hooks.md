# ADR-039: Model write hooks run on every write path

**Status:** Proposed (2026-10-21; design agreed by the project owner) ·
**Design doc:** §3.4, §9.4 · **Amends:** the intent format, the intent API,
ADR-025, ADR-031 · **Depends on:** ADR-018, ADR-030, ADR-036, ADR-037

## Context

Intent hooks (`validate`, `before_create`, `after_create`,
`before_update`, `after_update`, `before_delete`) belong to a CRUD
endpoint. Each other way of writing the same model ran none of them:

- a domain action (ADR-025);
- `Ctx::create`, `Ctx::update` and `Ctx::delete`, called from handlers,
  hooks and Rust operations (`Ctx::run`, ADR-031).

So a rule that must hold for every change to a model could be skipped by
using another write path. Two examples: "entering `won` needs an approval,
and consumes it"; "winning a deal makes its organization a customer".
`writable = "actions"` (ADR-037) closes this for one field, but only by
banning the other paths. Django's answer is the model's `clean`/`save`
overrides and `pre_save`/`post_save` signals, which every save runs.
Proseware, which generates `trestle.toml` files, needs the same in the
intent.

Hooks also had no way to serialize on a related row. `Ctx::get` doesn't
lock, and `Ctx::update` doesn't check a version.

## Decision

- **`trestle.toml`:** a model names its write hooks:

  ```toml
  [[models]]
  id = "crm.deal"
  hooks = { before_write = "crm::deal_rules", after_write = "crm::deal_effects" }
  ```

  Either point may be left out. Each name is registered with
  `Project::write_hook(name, f)` (or `Handlers::write_hook`), and linking
  checks it the way it checks handler and endpoint hook names: a name the
  file uses must be registered, and a registered name must be used.
  Endpoint hooks and write hooks are registered separately because their
  signatures differ.
- **`IntentDoc`:** `ModelDoc.hooks: BTreeMap<String, String>`, left out
  when empty. `ModelSpec.hooks: BTreeMap<String, Named>`. Runtime-built
  intents (ADR-030) set `ModelDoc.hooks` and register the functions with
  `write_hook` on the same `Project`, as they already do with `handler`
  and `hook`.
- **One signature:**
  `Fn(Ctx, Write) -> impl Future<Output = Result<Write, Problem>>`.
  - `Write::operation()` is the operation that made the write
    (`crm.deal.update`, `crm.win`), or `"ctx"` for a write through `Ctx`.
  - `Write::kind()` is `WriteKind::{Create, Update, Delete}`.
  - `Write::old()` is the row before the write, locked `FOR UPDATE`;
    `None` on create.
  - `Write::row()` is, in `before_write`, the row about to be written
    (on create, the values to insert, without `id` or defaults) and, in
    `after_write`, the row as written. It is `None` on delete.
  - `Write::set(field, value)` adds a change to the write. Only
    `before_write` may call it, and only on a create or update.

  The hook returns the `Write` it was given. Its `Ctx` has the same
  connection, actor and tenant.
- **Every write path runs them**, inside the write's transaction:

  | Path | Order |
  |---|---|
  | CRUD create, update | endpoint `validate`, then `before_create` or `before_update` → `before_write` → the write → `after_write` → endpoint `after_create` or `after_update` |
  | CRUD delete | endpoint `before_delete` → `before_write` → the delete → `after_write` |
  | Action | lock, `If-Match`, `repeat`, `when`, `require`, `check`, `exists` (ADR-036) → `before_write` → the write → `after_write`. An `"unchanged"` no-op writes nothing and runs no hooks. |
  | `Ctx::create`, `update`, `update_versioned`, `delete` | contract validation and the managed-field refusal → `before_write` → the write → `after_write` |

  Endpoint hooks come first because they belong to that endpoint's input.
  The model's rule then sees the row exactly as it will be stored, so it
  holds whichever endpoint produced it. Action guards run before
  `before_write` and aren't re-evaluated after it, so a hook's change is
  trusted code and isn't re-guarded.
- **Refusal.** A hook that returns a `Problem` refuses the write. The
  executor rolls back the whole transaction: earlier writes, the hook's own
  writes and nested hooks' writes. The problem is the operation's
  response, so a 4xx such as a 409 `approval_required` reaches the caller
  as it was returned.
- **Managed fields (ADR-037) stay with actions.** `Write::set` goes
  through `Row::set`, which refuses a field with `writable = "actions"`,
  as well as `id`, `version` and `created_at`, with the usual 500. A hook
  can't replace the row, so it has no other way to write one. An action's
  own `set` values reach `before_write` in `row()` and are written as
  before.
- **Nested writes and recursion.** A hook's `Ctx` writes run the hooks of
  the model they write. Each `Ctx` carries the chain of models whose write
  hooks are running. A write to a model already in that chain doesn't run
  that model's hooks again, whichever row it writes and however deep the
  chain is. This is why a deal's `after_write` that updates the
  organization runs the organization's hooks, and why an organization's
  `after_write` that writes its own row ends there. A chain deeper than 8
  (`MAX_HOOK_DEPTH`) is a 500 that rolls back. It can't happen while
  models don't re-enter, but it bounds the rule anyway.
- **`Ctx` additions,** both tenant-scoped in SQL and only in a write:
  - `get_for_update(model, id)`: the row locked `FOR UPDATE` until
    commit, or 404.
  - `update_versioned(model, id, expected_version, values)`: locks the
    row and compares versions, giving 404 for a missing row and the 412
    `precondition_failed` of a stale `If-Match` for an old version, then
    writes like `update` with the write hooks.

  An unversioned `Ctx::update` or `Ctx::delete` of a hooked model now
  locks the row `FOR UPDATE` first, so the hook gets `old()`.
- **Declared effects (ADR-018).** An operation that runs write hooks
  declares no effects, as a CRUD write with an endpoint hook already
  doesn't. That covers CRUD create, update and delete, and actions, on a
  model with write hooks. A hook is Rust that may write any model and
  enqueue any job. Effects are recorded rather than enforced for writes,
  and enforced for `emits`, so a hooked operation with a declaration
  would refuse its hook's jobs. Letting each hook declare the models it
  writes, and adding them to every operation that writes the hooked model,
  would be a second, unchecked declaration that describes code it can't
  see. A hand-written operation that writes a hooked model through
  `Ctx::run` declares effects that cover its hooks, or declares none.
  Operations on models without write hooks keep their declarations.

## Evidence

`crates/trestle/tests/intent_actions.rs` (PostgreSQL) uses a deal with an
unmanaged `stage`, an organization and an approval. The project is built
from an `IntentDoc` whose model hooks are added in code (ADR-030).
`before_write` on the deal lists an unconsumed approval of its
organization, locks it with `get_for_update` and consumes it with
`update_versioned`. Otherwise it refuses with 409 `approval_required`.
`after_write` marks the organization a customer through `Ctx::update`.
The organization's `after_write` counts its writes by writing its own row.

- `write_hooks_are_checked_when_the_file_is_read_and_linked` covers an
  unknown hook point, a non-string name, an unregistered name and a
  registered name that no model uses.
- `write_hooks_hold_on_every_write_path_in_its_transaction`:
  - effects: `crm.win`, `crm.deal.update` and `crm.organization.create`
    declare none, while `crm.approval.create` still declares
    `writes: ["crm.approval"]`;
  - creating an organization runs its `after_write` once: `writes` is 1,
    `last` is `crm.organization.create` and `version` is 2. The hook wrote
    its own model and didn't loop;
  - without an approval, the action, a CRUD `PATCH` of `stage` and a
    handler's `Ctx::update` are each 409 `approval_required`, and the deal
    is unchanged at version 1;
  - with one, the action wins. The approval is consumed and the
    organization becomes a customer through a nested write, which ran the
    organization's hook once (`writes` 2, `last` `"ctx"`);
  - a CRUD update refused by the endpoint's `after_update` returns 409
    `late_failure`. That hook only returns `late_failure` after it has
    seen the organization already a customer, so `after_write` ran
    before it in the same transaction. The approval, the organization
    and the deal are then all as before;
  - the same update without the failure wins. Its note reads
    `endpoint, model`: the endpoint's `before_update` set `endpoint`,
    and `before_write` extended it;
  - the handler with a stale version in `update_versioned` gets a 412,
    and the approval stays unconsumed. With the current version the deal
    wins.
- `two_wins_never_consume_one_approval_twice`: a test transaction holds
  the approval `FOR UPDATE`. Two wins of different deals of the same
  organization both wait on it, which shows up in `pg_stat_activity` as
  two lock waits. After the commit, one is 200 and the other is 409, and
  exactly one deal is won.
- `intent_doc.rs`: `every_key.toml` uses `hooks` on a model, and the
  document round-trips and assembles like its file.

Mutation checks, both caught:

- **Re-entering a model's own hooks:** the organization's hook recurses
  until the depth bound, and creating an organization returns 500.
- **`get_for_update` without the lock:** the losing win gets past the
  hook's check, then stops at `update_versioned`'s version comparison
  with a 412 instead of a 409. The version check is the second guard.

## Consequences

- **A rule written once holds for every path:** CRUD, actions, handlers,
  other hooks and Rust operations. A transition that needs Rust no longer
  needs `writable = "actions"` to be safe, although the two combine.
- **A hooked model's operations show no effects** in `inspect`, as
  operations with endpoint hooks already did.
- **An `after_write` that writes its own row** changes it after the row
  was written. The operation's response, and its `ETag`, show the row as
  first written. Prefer `Write::set` in `before_write` for changes to the
  row itself.
- **Hooks don't re-enter their own model,** even for another row of it.
  A rule that must also cover such writes has to make them itself.
- **Breaking for struct literals:** `ModelSpec` has a `hooks` member, and
  `ModelDoc` a `hooks` field, so literals need them or
  `..Default::default()` (for `ModelDoc`). `Handlers` has a new private
  map. Code that called `Ctx::update` or `Ctx::delete` on a model with
  write hooks now takes a row lock first.
