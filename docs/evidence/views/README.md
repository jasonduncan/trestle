# Admin and application views: local validation

Implementation baseline: `a59936cb73263a4340f4ffbce5a3cf74548d0361` (0.5.1).
The implementation is an uncommitted working-tree change. `source-manifest.json`
identifies its changed source files; the baseline SHA alone does not identify
the implementation. The original [handoff](../../design/Trestle_Admin_and_Application_Views_Engineering_Handoff.docx)
has SHA-256 `a9df6a46c9bef288d24af266fa736e5638031cb614be69d76706ed50dd15bde0`.

## Evidence

- `verification.json`: commands, results, local environment and test totals.
- `support-{empty,populated}.json`, `maintenance-{empty,populated}.json`:
  real PostgreSQL + Chrome runs, including renderer digest, baseline commit,
  schema digest, browser version, machine profile, initial row counts and checks.
- Matching PNGs: the empty database form, edited admin record and product workspace.
- `renderer.json`: exact shared example bundle's compatibility manifest.
- `compatibility.json`: executes the actual pre-views decoder from the baseline
  against unchanged legacy and extended current descriptors.
- `packages.json`: tarball installation outside the workspace, public declarations,
  CSS, production consumer build and scaffold frontend build.

Both fixture applications use the same compiled asset digest. Each starts with
10,000 primary rows, 125 related choices and 125 user/technician choices. The empty
case runs before seeding. Browser writes use the real executor. The response-loss
test forwards a PATCH to the API, lets it commit, then drops its response; closing
and reopening the form sends no replay. Test servers use allocated loopback ports
and are stopped afterward. Successful runs drop their databases.

The earlier `frontend/e2e/run-views.mjs` harness uses simulated API responses. It
is useful for frontend development but is not used as database evidence here.

## Acceptance coverage

These are traceability links, not a claim that every criterion has a separate
end-to-end test. Source/design checks are identified explicitly.

| Handoff IDs | Evidence and boundary |
|---|---|
| A01–A03 | `crates/trestle/tests/views.rs`: no exposure from models, invalid columns/filters/sources/record targets rejected offline. |
| A04, A13 | Registry mappings, contracts and presentation overrides enforce the read/write boundary. A presentation override cannot add an input or grant disclosure; applications must remove forbidden data from the authorized read projection itself. This responsibility is documented and source-reviewed. |
| A05, A07 | `frontend/client/test/views.test.ts`, existing protocol tests, and renderer pairing tests reject malformed metadata, unavailable features/components and incompatible component contracts. |
| A06, A08 | Pilot snapshot tests and `intent_doc.rs`; no changes to the pilot's published documents or generated SDK. |
| A09–A11 | `views_live.rs`, pilot resource tests and `intent_permission.rs`: non-owner staff, viewer refusal, hidden records/references and stale resolved-context rechecks. |
| A12, A42, A49 | Transport-generation test, controller invalidation tests and `admin-react/test/host.test.tsx`; access refusal quarantines the workspace, and failed session checks never fall back to development identity. |
| A14 | Mounted read sources keep independent representations, with no shared entity cache that can overwrite a full record with a list projection. This is a source-reviewed design property. |
| A15, A46 | Registered support availability read, mismatched observation decoder tests and existing state/version transition tests. Hints bind resource, record key and ETag and remain advisory. |
| A16 | Existing `intent_permission.rs` transaction tests fail after a write and verify that no row survives. No multi-request atomic save mechanism was introduced. |
| A17 | Existing pilot admin/resource and invocation tests keep all entry points on the same executor; live fixture forms exercise that path. |
| A18–A21 | Controller status tests, form conflict tests, pilot precondition tests and live Assign/Complete/export flows. Export returns a 202 receipt while checking a separate case representation's actual ETag. |
| A22, A27, A28, A40, A51 | Immutable-attempt unit tests and the committed-response-loss browser test; generic 5xx, malformed success and abort/loss remain unknown and cannot create another dispatch from the same handle. |
| A23–A26 | Existing receipt concurrency/expiry/reauthorization tests, new stored-projection refusal test, operation-version tests over HTTP/direct invocation, and denied read-only reconciliation test. No execution-capable recovery API is exposed. |
| A29–A32 | Both empty/populated browser runs; selected-value resolution beyond the first page, search, native keyboard selection/save, bounded pagination; delayed filter-query regression test. |
| A33–A36 | Shared compiled custom widget/section bundle, real background job/status concealment, external package/scaffold consumer and matching renderer digests. |
| A37–A38 | Stable override tests survive an upstream title change and refuse removed column targets. |
| A39 | Live inputless DELETE confirmation uses the checked invocation adapter and the same controller, without inventing a form revision. |
| A41 | Explicit source identity and record-input mappings are assembled and decoded against contracts. Unsupported mappings fail before serving. The main fixtures use UUID `id`; arbitrary projection identities are supported by the public builder and are source-reviewed rather than a separate reference app. |
| A43–A45 | Live actor/tenant/page-size cursor isolation and HTTP/direct version refusal; cursor shape also binds normalized query and projection digest. Form/view revision mismatches refuse invocation; stale-form tests retain compatible values. |
| A47–A48, A50, A52 | Actual old-decoder compatibility test; production host tests for signed-out, zero/one/multiple tenants, empty views and old-backend refusal; scaffold and minimum intent-admin live runs. |

## Reproduction and release boundary

Use the commands in the [implementation guide](../../guide/views-implementation.md).
The full workspace Rust suite, focused new regressions, Clippy, formatting,
frontend tests/typechecking, production builds, package/scaffold checks and live
browser runs are local validation. CI has been configured to retain equivalent
browser evidence; a remote CI run is not claimed.

Existing schema history is unchanged and `makemigrations --check` is clean.
The existing schema-fence test covers an older writer after a fenced migration;
this change does not weaken it. Legacy interaction bytes/revisions are preserved.
Generated cursors from the old shape require a fresh page after upgrade.

The npm packages remain local candidates. Real-host session/origin and edge
configuration, SMTP/OIDC deployment qualification, external security review,
publication and owner pilot approval remain separate. The durable export is an
authorized JSON preview in the fixture application; it does not claim file-storage
or delivery integration. Accessibility evidence is the focused native-keyboard
flow, not a comprehensive accessibility audit.
