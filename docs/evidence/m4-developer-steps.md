# M4: developer steps, measured

Design doc §19.3 asks for "the code and steps needed to add the second
resource, add an input constraint, expose a domain action, and change a
supported form field", with less duplicated plumbing, not the fewest lines
at any cost. Numbers below were measured in this repository at M4
(non-blank, non-comment lines).

## 1. Adding the second resource (`units`)

| What | Hand-written | Generated or derived |
|---|---|---|
| Model (persistence shape) | 7 lines (`unit_model`) | migration `units/0001_initial` (10 lines of SQL) via `pilot makemigrations --name initial` |
| Wire contracts | 55 lines (4 types + builder contracts) | JSON Schema, OpenAPI components, TS types |
| Persistence | 32 lines (typed table, one scoped check) | — |
| Policy | 25 lines (reuses `schools::recheck_role`) | — |
| Operations | 109 lines (list with signed cursor, create, get) | — |
| HTTP binding | **4 lines** (`app.resource(..).list(..).create(..).retrieve(..)`) | 3 managed routes with Location/ETag, problem+json, 3 OpenAPI operations, 3 TS client functions, route inventory entries |
| App wiring | 2 lines (install app, migration ref) | lifecycle order and dependency checks |

Steps: write the model, then `pilot makemigrations`; write contracts,
policy and operations; register the resource; `make snapshots` and review
the diff. There is no hand-written handler, router entry, Location header,
problem mapping, OpenAPI entry, SQL DDL or client code.

For contrast, the same repository still contains hand-written Axum paths:

- a create handler that calls the executor directly: 8 lines per endpoint,
  plus routing (`examples/pilot/tests/conformance.rs`);
- the CSV export route: 36 lines for one GET.

These are correct because they reuse operations, but each one gets no
OpenAPI entry, no generated client and no form or admin metadata. That
per-endpoint plumbing is exactly what resources remove.

## 2. Adding an input constraint

Example: `lessons.create.input` title length. One edit
(`Field::text("title").chars(1, 200)`) changes server validation, JSON
Schema, OpenAPI, the TS types, browser validation and every form using it.
`make snapshots` shows the published diff, and the shared fixtures fail
until they agree. The M1 mutation check (200 → 199) is caught by
`conformance.rs`.

## 3. Exposing a domain action

`lessons.publish`: the operation (about 35 lines of behavior) plus **one**
registration line, `.action("publish", PublishLesson)`. That produces
`POST /api/v1/lessons/{id}/actions/publish`, the OpenAPI entry, a
`lessonsPublish` client function (used by the product screen) and, with
`.action_when(...)` in the admin view, an admin button. Both clients hit
the same operation (`docs/evidence/m4-demo.json`, audit rows for both
actors).

## 4. Changing a supported form field

Adding the relation picker to `lessons.create.form` was one line
(`.relation("/unit_id", "Unit", "units.list", "name")`). There is no
migration (presentation is not schema) and no frontend rebuild for
metadata-driven forms (M2 evidence). The descriptor's `requiredFeatures`
gained `forms.relation@1`, so older bundles refuse instead of dropping
the field.

## Caveats

- Operations and contracts are still explicit Rust. A contract derive would
  shrink the 55 contract lines. Generic CRUD operations were not built:
  with two resources, the only shared behavior (cursor paging) was
  extracted into `pagination::page_from`, and nothing else repeated
  enough to earn an abstraction (§17.3).
- These are counts from one codebase and one developer, not a user study.
