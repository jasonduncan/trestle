# Intent parity: pilot's `units` app

`examples/pilot/intent/units.toml` declares pilot's hand-written `units` app
in about 20 lines of TOML. `examples/pilot/tests/intent_parity.rs` checks it
two ways against pilot's own migrated database:

- **Schema.** `verify` compares the intent's desired schema (`schools`,
  `school_memberships` and `units`) with the catalog that pilot's
  migrations produce, and finds no mismatches. The table names, columns,
  types, nullability, primary keys, foreign keys, unique constraint and
  check constraint names all match.
- **Behavior.** Both routers run over one database and give the same
  results for the core cases:
  - a teacher creates a unit (201)
  - a duplicate name in the same school is a 422 on `/name` with code
    `already_exists`, while another school may reuse the name
  - a viewer can't write (403)
  - an empty name is rejected (422)
  - another school's unit is a 404
  - a non-member of the selected school gets a 403
  - each app lists, in name order, the units the other one created

This needed one intent feature: `unique = [["name"]]` on a model. It is
scoped per tenant and named `<table>_<tenant>_<fields>_key`.

## Differences (why pilot still serves the Rust app)

Swapping the served app would change pilot's published API, so it stays a
decision for later:

| | Rust app | Intent |
|---|---|---|
| Operation IDs | `units.list`, `units.create`, `units.get` | `units.unit.list`, `.create`, `.get` |
| `version` in responses | string of digits (`"1"`) | integer |
| List paging | signed keyset cursor | `limit` only |
| Audit | records an audit event on create | none (it would need an `after_create` hook) |
| Admin profile | product and admin audiences | product only |
| Error message wording | "...already exists in this school." | "A unit with this name already exists." |

Pilot's other apps (lessons, audit) use features the intent format
doesn't have yet: relations, domain actions and forms. They stay Rust apps,
which the design expects (§3.4).
