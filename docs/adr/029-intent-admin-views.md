# ADR-029: Admin views for `trestle.toml` apps

**Status:** Proposed (2026-10-17; design agreed by the project owner) ·
**Design doc:** §7.1, §10 · **Amends:** the intent format, ADR-026 ·
**Depends on:** ADR-014, ADR-025, ADR-026

**Extended by [ADR-033](033-admin-and-application-views.md):** existing declarations
lower to collection/record views with the same policies and legacy audience
behavior; the reusable admin package consumes the separate view protocol.

## Context

The admin shell renders resources that Rust apps register with
`app.resource(..).admin(view, policy)`. Its verbs took only typed
`Operation`s, and its policy had to be a typed `Policy<C>`, so an app built
from `trestle.toml` could not appear in the admin shell. While checking
this, a gap in ADR-026 came up: runtime-built operations had no
availability function, so `/_meta` never served the forms generated for
them.

## Decision

- **Resource verbs take either kind of operation.** `list`, `create`,
  `retrieve`, `update`, `delete` and `action` take `impl ResourceOp<S>`,
  which a typed `Operation` and an `Arc<dyn DynOperation<S>>` both
  implement. Bindings, ETags and checks are unchanged.
- **`action_at(name, op, http)`** puts an action at a binding the caller
  chooses, such as the path an intent file declares.
- **`DynPolicy<S>`** is a runtime-built policy: a context descriptor,
  context resolution and `admit`, the same parts as a `DynOperation`
  without the operation. `admin_dyn(view, policy)` publishes the admin
  profile with one. `admin_policy()` now returns `Option<&str>`, because a
  runtime policy owns its ID (ADR-014: no leaks).
- **Runtime-built operations get availability** from their own context and
  `admit`, like typed ones. Generated product and admin forms are now
  served over `/_meta`, with `availability`.
- **Admin actions bound with `If-Match`** are described with
  `"ifMatch": {"pointer": "/version", "etagOf": "<output contract>"}`, and
  their body binds no version. The shell sends `If-Match:
  "<etagOf>:<version>"`, the ETag the server issues. Actions that take the
  version in the body are described as before.
- **`trestle.toml`:**
  - **Every CRUD endpoint is a resource** (ID = the model ID), with the
    conventional bindings it already had.
  - **`[[admin]]`** adds that resource to the admin profile:

    ```toml
    [[admin]]
    model = "crm.deal"
    permission = "crm.admin"   # who sees it
    title = "Deals"            # default: the table name in words
    columns = ["title", "stage"]   # default: every field
    filters = ["stage"]        # default: the endpoint's filter
    actions = ["crm.win"]      # action endpoints on the model
    ```

  - **Record name and forms.** The model's `label` names a record (`id`
    when there is none). Create and update get admin-audience forms,
    `<model>.admin_create.form` and `<model>.admin_edit.form`, built like
    ADR-026's, so every field needs a widget.
  - **Action buttons.** Each listed action becomes a button named after its
    last segment ("Win"). When `when` names a single field with a single
    value, the button shows only for rows in that state (advisory).
  - **Audiences.** The model's operations and its listed actions join the
    admin audience.
  - **Checked when the file is read:** the model has a CRUD endpoint with
    `list`; the permission is required and granted by a role; columns are
    item fields; filters are among the endpoint's; actions are action
    endpoints on the model, in its app; at most one section per model.
- **Showing grants nothing.** The admin permission only decides who sees
  the view. Every button and form submits to the same operation, which
  checks its own permission.
- **The admin shell** shows money cells as "12.50 USD". Its row helpers
  live in `frontend/admin/src/rows.ts`, with tests.

## Evidence

- **`crates/trestle/tests/intent_admin.rs`:**
  - **Offline:** every CRUD endpoint is a resource, and only the `[[admin]]`
    model is in the admin profile. The description has the title, label,
    columns, default filters, bindings, forms, and the action with
    `ifMatch` and `visibleWhen`. The admin forms are admin-audience, and the
    audiences are as described.
  - **Checks:** each check above, plus a uuid field the admin forms cannot
    show, even without `forms = true`.
  - **PostgreSQL:**
    - an admin gets the manifest, and a member without the permission gets
      403;
    - a clerk who sees the view but cannot write gets the edit form with
      `available: false, reason: permission_denied`;
    - a generated product form is served with availability;
    - the shell's button flow (list, then POST with `If-Match` from the row)
      wins the deal for an admin and is a 403 for the clerk.
- **`frontend/admin/test/rows.test.ts`:** money cells, and the body or
  `If-Match` header an action sends.
- **Existing suites are unchanged:**
  - intent CRUD, actions and forms;
  - pilot's resources and published snapshots;
  - the scaffold check;
  - downstream commons and smart-folder.
- **Mutation checks, 13 of 13 caught:**
  - no availability for runtime-built operations;
  - a runtime admin policy admitting everyone, or the view shown to any
    signed-in caller;
  - `ifMatch` missing, or the version also bound in the body;
  - operations left product-only;
  - no `visibleWhen`;
  - admin forms skipping the widget check;
  - listed actions registered twice;
  - filters defaulting to none;
  - another model's action accepted;
  - the shell ignoring `ifMatch`, or showing money as an object.

## Consequences

- **Intent apps reach the admin shell** without Rust. The parity guide no
  longer lists admin views as Rust-only.
- **Generated forms now actually serve.** Before this ADR, `forms = true`
  published descriptors that only the registry and exports could see.
- **One resource per model.** Handler endpoints stay outside resources;
  wrap them in Rust if the admin needs them.
- **Admin actions take no input** beyond the row and its version, as the
  shell supplies nothing else yet. Intent actions' `accept` fields are
  optional, so they qualify.
