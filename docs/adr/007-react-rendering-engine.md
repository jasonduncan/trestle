# ADR-007: React rendering engine and overrides

**Status:** Accepted (M2, 2026-09-26) · **Design doc:** §9, §11.2

## Context

The design asks us to evaluate JSON Forms against the exact fixtures
rather than build a universal renderer.

## Measured

`frontend/interaction-react/test/ajv-evaluation.test.ts` runs the shared
fixtures through AJV (JSON Schema 2020-12), the validator JSON Forms uses:

- AJV agrees with the server on **accept/reject for all 26 fixtures**.
- AJV cannot reproduce the server's 400-vs-422 split or Trestle's stable
  codes and messages. Unknown fields surface as `additionalProperties`, a
  null for a non-nullable field as `type`, and so on. Mapping those back is
  hand-written glue.

## Decision

Own a small renderer, `@trestle/interaction-react`:

- a validator derived at runtime from the descriptor's input schema that
  implements `trestle.portable@1` (it agrees with the server on every
  fixture, including codes and pointers);
- a local widget registry (`text`, `textarea`, `integer`, `checkbox`,
  `select`) that applications can extend or override by key;
- problem+json mapping onto fields, stale-revision reload that never
  resubmits by itself, and an unknown-outcome message instead of a retry;
- accessibility basics: labels and error descriptions wired with
  `aria-describedby`, a focused error summary, and an `aria-live` status.

## Why not JSON Forms now

- Its UI schema differs from our presentation format, so we would maintain
  a translator.
- Validation codes, the 400/422 split, 409 stale handling and
  availability would all be custom code around it anyway.
- It pulls in a renderer set whose accessibility and security posture we
  would also have to verify. The M2 widget set is five controls.

Revisit when forms need layouts, arrays or conditional rules beyond the
small declarative rule language in §9.3. The fixture suite makes that swap
testable.

## Evidence

49 renderer tests: fixture agreement, decoder safety (external references,
origin escape, `__proto__` sent as JSON, limits), component behavior
(local validation, submission with revision, a new optional field with no
code change, 409 refresh, 403/401 messages, network failure, feature and
widget gating, text-only labels). There is also the real-browser run in
`docs/evidence/m2-demo.json`.
