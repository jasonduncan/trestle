# Admin and application views implementation

Source: [engineering handoff](../design/Trestle_Admin_and_Application_Views_Engineering_Handoff.docx), version
1.0, 18 October 2026. Implementation starts at `a59936c` (Trestle 0.5.1),
after the handoff's `bc372db` baseline. The handoff is a product specification;
its illustrative signatures are not executable instructions.

## Delivery sequence

1. P0: versioned view documents, checked registry bindings, shared fixtures,
   deterministic export, unchanged legacy interaction bytes.
2. P1: shared immutable submission attempts, routed generic admin, compiled
   packages, external consumer and scaffold.
3. P2: action forms, explicit preconditions and version pinning, relationship
   lookup, layout, contextual availability and security generation invalidation.
4. P3: composed records, queue, custom slots, work status, presentation overrides
   and two unrelated reference applications.
5. P4: compatibility and browser evidence, production builds, release guidance.

Each completed capability must have executable evidence. Local validation does
not grant pilot approval, security approval, publication or deployment authority.
Unknown command outcomes never permit automatic replay. Generic receipt-only
recovery remains outside this release.

## Using the admin

`trestle new demo --admin` writes the API project and a separate `frontend/`
host. The starter explicitly registers Notes admin and a bounded authenticated
workspace directory. It uses the existing browser sign-in flow. See the generated
frontend README for package installation, account provisioning and origin setup.
The packages are local release candidates, not published npm packages.

An existing intent application needs its normal endpoints plus an explicit admin:

```toml
[[admin]]
model = "crm.account"
permission = "crm.admin"
title = "Accounts"
columns = ["name"]
```

Grant the permission through roles. Admin publication controls discovery; read
and write permissions independently control execution. CRUD is opt-in, and the
runtime only adds create/edit/delete buttons for actual registered bindings.
An action with editable inputs must name a form; generated intent actions use
`forms = true` and their `accept` fields. A readonly user can discover a view
without gaining permission to invoke its writes.

Use `AdminHost` for sign-in and tenant discovery, or embed `AdminShell` with an
application-owned transport. `AdminHost profile="product"` shows the application's
own (Product) views instead of the admin shell's; the built `frontend/admin` takes it
from `VITE_ADMIN_PROFILE=product`. `AdminShell` accepts `profile="product"`, custom
`widgets`, and build-time `components`/`sections`. These extension functions are
compiled imports, never URLs supplied by metadata. `frontend/views-demo/main.tsx`
contains a custom text widget and a custom export panel using public APIs.

## Application views

Intent projects can declare `[[views]]` with `kind`, `audience`, `permission`,
`query`, `identity`, `label`, `columns`, `filters`, `record_view`, `record_input`,
`actions`, `action_input` and `sections`. See `fixtures/views/support.toml` and
`maintenance.toml` for complete, assembled examples. The every-key intent fixture
checks public `IntentDoc` roundtripping through the existing parser.

For Rust apps, use `ViewDefinition::{collection,record}`, `SectionDefinition`,
`AppBuilder::{view,view_dyn}` and `PresentationOverride`. `.action_with_context`
maps named operation inputs to record pointers. A multi-parameter action cannot
silently map every parameter to the same identity. A section's `component` key
declares a compiled extension and `required_component` blocks the view when that
extension is absent. Optional sections use their typed table fallback.

Each query must already be registered as `Mode::Read` and published to the view's
audience. The output contract defines its projection. A narrower list projection
never replaces a full record. Inputs come from the operation contract; view
columns cannot add writable fields. Related reads are independent snapshots.
Supply one composite read operation when the application needs a common snapshot.

A Rust collection over a read that caps its rows can name the output's
completeness flag with `.complete("complete")` (ADR-045). The field must be a
required boolean beside `items`. When a page returns `false`, the collection says
"This list is incomplete: more rows exist that it doesn't show." The view then
requires `views.completeness@1`, so an older renderer refuses it instead of showing
a list that looks whole. A record view's related sections carry no flag yet.

Rust views can call `.availability(query, input_mapping)` to attach an explicit
authorized read using `trestle::protocol::view::availability_contract`. Its
managed GET binding supplies bounded hints with the resource, record key and
actual representation ETag. The shell requests it once per record observation,
rejects mismatched hints and disables commands until the record is refreshed.
The support fixture demonstrates it. New explicit sources need the declared
audience; legacy `AdminView` declarations keep their already-validated audience
behavior for compatibility.

An endpoint's optional `search = "name"` adds the bounded `q` input. Forms that
reference a searchable model with a retrieve operation receive the new remote
lookup editor: search, next page and resolution of the current selection.
Models without this opt-in retain their legacy relation descriptor unchanged.

`FormPresentation::layout(LayoutNode::...)` composes named sections and columns
over already-declared fields. A field appears exactly once; depth, node count
and column count are bounded. A missing required editor or protocol feature
blocks submission. No field-specific browser validation is hand-written.

## Build and inspect

```sh
cargo run -p pilot -- inspect views
cargo run -p pilot -- views export --audience admin
cd frontend
npm ci
npm test
npm run typecheck
npm run build
npm run test:packages
npm run test:compatibility
npm run build:views
cd ..
cargo run -p pilot -- check --renderer frontend/admin-react/dist/renderer.json
TRESTLE_VIEW_BROWSER=1 cargo test -p trestle-framework --test views_live
```

`build:packages` emits ESM, declarations, source maps and the exported stylesheet.
`test:packages` packs all three packages, installs them in a temporary directory
outside the workspace, typechecks public imports and builds a consumer. No npm
publication occurs. `build:views` emits the shared example bundle and a renderer
manifest including both custom section keys and their accepted input contracts.

The live test creates and drops its own `trestle_test_*` databases. It starts a
loopback API and a static/proxy server on dynamically allocated ports, runs Chrome,
and shuts them down. It does not use the developer's API port. Reports and
screenshots go to `frontend/test-results/views-live/`. The fault test lets a real
PATCH commit, then drops its response and verifies that remounting sends no replay.

The earlier `test:views` harness uses registry-generated metadata and simulated
API replies. Its evidence is labeled accordingly; it is not a database test.
Shared descriptors are refreshed only by:

```sh
UPDATE_VIEW_FIXTURES=1 cargo test -p trestle-framework --test views
```

Review the diff. This does not rewrite the pilot's published interactions or SDK.

## Upgrades and operation

Ship static assets and JSON routes behind the same origin. Configure the real
browser sign-in method and an authorized tenant directory; a tenant header
selects a membership and grants no authority. Keep API/metadata responses private
and no-store. The production reference host excludes development token settings.

Pair the backend's exported requirements with the exact frontend build manifest.
Unchanged legacy forms retain their descriptors/revisions. Old bundles refuse
required new features; the new view shell on an old backend reports that the
workspace cannot be loaded. Existing legacy product forms continue to work.
A view/form revision mismatch refuses the action instead of combining releases.

Generated cursors from the previous release fail closed after this upgrade;
restart collection paging. Existing schemas and applied migrations are unchanged.
Rollback the frontend and backend together if the new view host must be removed.
Applications with additional migrations must still follow the existing schema
fence and migration runbooks; view metadata is not a rollback escape hatch.

On conflict, preserve the draft and inspect current data. On an uncertain write,
use the Activity entry and authorized record inspection. Closing/reopening the
dialog does not reset it. An application may supply read-only reconciliation
through `OperationController.reconcile`; its interpreter must use authoritative
attempt evidence. A different record value alone does not prove failure. Never
present ordinary idempotent invocation as status recovery.

The reference support job creates a durable authorized JSON export preview, with
a status read concealed from other actors and tenants. It stores only an export
ID in the job payload and reauthorizes the worker's operation. Real file delivery,
provider integrations and durable cross-session recovery are separate features.
