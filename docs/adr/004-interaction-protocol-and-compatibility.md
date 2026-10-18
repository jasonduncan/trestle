# ADR-004: Interaction protocol, features and compatibility

**Status:** Accepted (M2, 2026-09-26) · **Design doc:** §8, §11.3–§11.4, App. B, I03, I11 ·
**Amended by:** ADR-022 (decimal, date, instant, duration)

## Context

M2 must render a form from backend metadata, survive a backend release
while a browser tab is open, and never let metadata confer authority.

## Decision

- **Descriptor:** `trestle.interactions/1.0` with the Appendix B shape
  (renamed). It is generated once at registry assembly from the form, the
  operation binding and the contracts. `revision` = `r-` + 16 hex digits of
  the descriptor's canonical digest, so any change a renderer could observe
  yields a new revision.
- **Publication:** `GET /_meta/v1/manifest` and
  `GET /_meta/v1/interactions/{id}`.
  - Seeing a descriptor requires the bound operation's request context
    (authentication plus tenant membership).
  - `availability` comes from `Policy::admit` and is advisory.
  - All responses, problems included, are `Cache-Control: private,
    no-store`.
  - Only the product audience is served; admin gets its own profile in M4.
- **Revision check:** managed routes compare `Trestle-Interaction-Revision`
  with the operation's accepted set (current form revisions plus explicitly
  retained ones) after authentication and before reading the body.
  - Unknown revision: 409 `interaction_stale`, with no effect.
  - No header (plain API client): no check.
- **Retention is explicit and verified.** A form lists retained revisions
  with a reason (`app.form(..).retain(rev, reason)`).
  `examples/pilot/published/revisions.json` records every published revision
  with its input contract IR, and `make snapshots` only appends to it. A test
  proves the current input contract accepts every instance a retained
  revision could send (`ContractDoc::rejects_instances_of`).
- **Features:** `requiredFeatures` lists what a renderer must implement.
  Unknown required features block submission. Unknown minor protocol
  versions decode; an unknown major version is rejected.
- **Renderer-side safety** (the decoder in `@trestle/interaction-react`):
  - enforces the §8.5 limits and a closed JSON Schema subset;
  - rejects all reference keywords and prototype-polluting names;
  - keeps bindings under `/api/`;
  - renders labels as text.

## Evidence

- `examples/pilot/tests/meta.rs`: visibility, availability, no-store,
  served descriptor equal to the export, 409 before any effect, and
  revocation between load and submit.
- `docs/evidence/m2-demo.json` (a real browser; `make demo-m2`):
  - release A's open form submits to release B, whose retained revision
    `r-acbd164ff4e78da6` is accepted;
  - the same bundle (digest recorded; reloaded assets byte-identical)
    renders B's new optional field and submits it;
  - revocation is rejected;
  - the two tenants stay isolated;
  - an unaccepted revision gets 409.
- Mutation check: making B's subtitle required fails the retention test
  with "new field subtitle is required".

## Consequences

- Retaining a revision is a reviewed code change, and the tooling refuses
  false claims of structural compatibility. Semantic compatibility (same
  meaning) is still the developer's judgment, stated in the reason.
- Label-only changes also produce new revisions. Retain the old one if open
  forms should keep submitting; otherwise they refresh on 409.
- Contextual descriptors cannot be CDN-cached. Content-addressed public
  caching needs an exposure review (§8.5).
