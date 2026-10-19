# ADR-008: Resource abstraction and raw Axum boundary

**Status:** Accepted (M4, 2026-09-26) · **Design doc:** §7, §10, §11.2, §12.3, I01, I04, I13

**Extended by [ADR-033](033-admin-and-application-views.md):** checked view
bindings, action forms and explicit concurrency targets preserve this boundary.

## Decision

- **Resources are binding conventions, not behavior.**
  `app.resource(id, path)` exposes operations you wrote:

  | Verb | Route | Convention |
  |---|---|---|
  | `.list` | `GET path` | `Page<T>` with a signed cursor |
  | `.create` | `POST path` | 201 + `Location` + ETag |
  | `.retrieve` | `GET path/{id}` | ETag |
  | `.update` | `PATCH path/{id}` | `If-Match` → `expected_version`; 412/428 |
  | `.delete` | `DELETE path/{id}` | `If-Match`; 204 |
  | `.action(name, op)` | `POST path/{id}/actions/{name}` | — |

  CRUD is opt-in per verb. Domain transitions are explicit actions, never
  writable flags (§10.2).
- **One representation per resource.** `If-Match` is checked against the
  operation's output contract, so assembly requires update and delete to
  return the retrieve representation. This rule came from a real bug found
  by `delete_requires_the_current_version`.
- **Inputs are assembled by the adapter:** path parameters, the query (GET
  and DELETE) or the JSON body, and the `If-Match` version. Each name has
  exactly one source (400 `conflicting_input`). URL text is converted to
  integer or boolean only when the contract says so.
- **Lists** return `{items, next_cursor}` pages (`Page<T>`, contract
  `<item>.page`, at most 100 items). Cursors are HMAC-signed, bounded,
  bound to the query shape (400 `cursor_mismatch`) and carry no authority.
  Keyset paging uses `(sort, id)` with a direction-matched tie-breaker.
  Filters are contract fields, so anything else is `unknown_field`.
- **Relations:** a `relation` widget names a list operation. Options come
  from that scoped operation, and writes revalidate the chosen ID. Another
  school's unit gets the same 422 `/unit_id not_found` as no unit at all.
- **Patch:** `Patch<T>` keeps Missing, Null and Value apart. The contract
  decides nullability, and forms send only changed fields.
- **Admin is a publication profile.**
  - `.admin(AdminView, policy)` publishes columns, filters, forms, actions
    and advisory `visibleWhen` rules at `/_meta/v1/admin/*` to callers the
    admin policy admits (pilot: owners).
  - Admin forms are ordinary forms with `Audience::Admin`.
  - The admin shell (`frontend/admin`) renders only that metadata, and
    every button calls the same operation a product screen would.
- **Generated TypeScript client:** `pilot sdk export`. It is snapshot-checked
  in `frontend/product/src/generated/api.ts`, typechecked, and used by the
  product's custom screens.
- **Raw Axum stays available.** `app.unmanaged(method, path, handler,
  waiver)` mounts any handler. It gets no contract, OpenAPI entry or
  generated client, and its waiver appears in `pilot inspect routes`. The
  CSV export shows the intended pattern: the same context extractor, plus
  operations called through the executor.

## Evidence

- `examples/pilot/tests/resources.rs`:
  - cursors (paging, tampering, reuse with another query);
  - allowlisted filters and bounded pages;
  - patch Missing/Null/Value with 428/412;
  - forbidden fields (`status`, `version`, `school_id`, `created_by`) and
    `id` in the body;
  - hidden relation targets in the picker, create and patch;
  - publish conflicts (`stale_version`, `invalid_transition`) and delete
    preconditions;
  - the owner-only admin profile using the same operations (audit rows);
  - the CSV route reusing `lessons.list`.
- `frontend/interaction-react/test/edit-and-relations.test.tsx`: the picker,
  edit forms sending only changed fields with `If-Match`, and stale edits.
- `docs/evidence/m4-demo.json` (real Chrome, `make demo-m4`):
  - the product relation picker hides the other school's unit;
  - product and admin publish through the same route, with audit rows for
    both actors;
  - an admin stale edit gets 412; a fresh edit sends only `subtitle`;
  - teachers cannot open the admin shell.
- `docs/evidence/m4-developer-steps.md`: 4 lines of HTTP binding for a new
  resource; migration SQL, routes, OpenAPI and client functions generated.

## Alternatives rejected

- **A generic model-to-CRUD viewset** would publish writable fields because
  columns exist (violates I01), hide policy per verb, and make the admin a
  backdoor.
- **Record-contextual descriptors** (`/_meta/v1/resources/{id}/records/{id}/interactions`)
  are deferred. The admin shell supplies record context (path, ETag,
  values) from the retrieve response instead, and the server revalidates
  everything.
