# ADR-033: Admin and application views

**Status:** Implemented locally; release and pilot approval remain separate.
**Date:** 2026-10-18. **Amends:** ADR-008, ADR-013, ADR-016, ADR-026, ADR-029.

## Decision

Keep view registration in `trestle-core::registry`, portable descriptions in
`trestle-protocol::view`, and browser behavior in three built ESM packages:
`@trestle/client`, `@trestle/interaction-react`, and `@trestle/admin-react`.
No backend HTML serving or new framework crate is introduced.

`trestle.views/1.0` has collection and record documents. A source contains its
registered operation identity/version, checked HTTP binding, exported input and
output contracts, and explicit row identity. Related sections reference other
registered reads and map named inputs from the parent projection. They load on
demand. Collection pages are bounded at 100 rows; metadata at 256 KiB, 32 columns
and 64 presentation nodes. The flat section graph has no recursive view imports.

`ViewDefinition` is explicit. Existing `AdminView` registrations lower to
collection/record views with their original publication policy. Installing a
model publishes nothing. Read projections are independent of writable inputs.
Every operation still enters the existing executor. Static metadata includes no
policy IDs; HTTP metadata is private and no-store. Publication grants no command
authority. Rust views can attach an explicit read-only availability operation
whose output uses `availability_contract`. Its input mappings and audience are
checked at assembly. It runs at its normal managed GET binding through the
executor; the proposed additional metadata wrapper route is unnecessary. Its
resource, record key and observed ETag must match the displayed record before
the client uses its bounded action hints. Legacy state hints use the authorized
record. Both remain advisory; commands check current state and version inside
their transactions.

New endpoints are `/_meta/v1/ui/manifest` and `/_meta/v1/ui/views/{id}`. Record
routes in the frontend use the view ID and explicit input mapping in the URL
fragment. Native tables, selects, dialogs and browser history keep the first
implementation dependency-free beyond the existing React/Vite stack. Native
dialogs restore focus; dirty forms guard navigation and dismissal.

## Compatibility and identity

Legacy interaction documents remain byte-identical. New layouts, relationship
search/resolve, explicit concurrency targets and version expectations opt into
interaction 1.1 with required features. Relationship descriptors carry both
resolved read bindings and their contracts. A view references the exact form
revision and refuses mixed metadata revisions.

Canonical JSON uses the existing protocol digest implementation. View revision
calculation blanks only its own revision before hashing; all resolved query,
contract, binding, form and presentation dependencies participate. Overrides
select or relabel existing stable presentation IDs and fail on removed targets.
`check --renderer <file>` compares protocol versions, features and required
component keys/input contracts. The build manifest digest covers compiled JS/CSS
and package metadata. It is a build identity, not a remote attestation mechanism.

`Http::precondition_target` names a registered retrieve operation, its
representation, identity input mapping and the expected-version input. The
browser obtains that read's actual ETag. Commands may return a different receipt
contract. `Http::version_pinned` advertises `Trestle-Operation-Version`;
`Invocation::with_operation_version` enforces the same expectation after context
resolution and before execution/receipt selection. Legacy callers omit it.
Retained keyed responses are validated against the current output contract after
reauthorization. An obsolete projection is refused without revealing its values
or entering the fresh command branch. Application policies remain responsible
for semantic disclosure rules that do not change a contract's shape.

Generated intent searches are explicitly enabled with `search = "text_field"`.
The input `q` is bounded to 200 characters and matched as a case-insensitive
literal substring, with bound SQL values after tenant scope. Generated cursors
bind actor, tenant, operation version, output projection, normalized search,
filters, order and page size. Existing generated cursors require a fresh page
after upgrading. Signing does not hide their authorized ordering values.

## Attempts and access changes

`OperationRuntime` owns immutable, in-memory, single-dispatch attempts. Forms and
buttons subscribe to the same controller. A prepared handle is consumed before
the first await; repeated execution returns the original promise. Only declared
idempotent bindings receive a generated key, fixed for that attempt. Malformed
success, generic 5xx and lost responses remain unknown and block fresh submission
for that flow, including after dialog remount.

Session activity retains unresolved attempts and one-time results until
dismissed. Permission refresh invalidates protected data and prepared handles,
while retaining unresolved attempt tombstones. Sign-out clears session content.
Transports cancel reads and discard late replies; the host uses separate runtime
scopes for actor/tenant combinations. Credentials are never cache keys or stored
drafts. The current query layer keeps each mounted source independently; it does
not install a persistent or shared browser data cache.

Reconciliation accepts only a registered read and an application interpretation
of authoritative attempt evidence. A refusal of that read cannot settle the
original attempt. There is no automatic replay, receipt-only lookup, localStorage
draft persistence or cross-session recovery. `committed_unreadable` is reserved
for an explicit future commit confirmation; an ordinary malformed reply is
classified conservatively as unknown.

## Evidence and boundaries

The guide and `docs/evidence/views/` record the validation performed. The support
and maintenance fixtures share a compiled renderer, custom field widget and
extension bundle. Each uses 10,000 primary records and 125 related choices, plus
an empty-database pass. Support queues a durable export preview through the
existing jobs executor; its status read checks both tenant and originating actor.
It exports an authorized JSON snapshot, not a file-storage subsystem.

This change requires no migration for existing applications. The fixture export
model/job table are created only in disposable test databases. Package publication,
deployment, external security review and pilot acceptance are not implied by
passing local tests. Boards, history, bulk actions and generic receipt recovery
remain follow-on work as specified by the handoff.
