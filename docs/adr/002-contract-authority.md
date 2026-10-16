# ADR-002: Contract authority and schema toolchain

**Status:** Accepted (M1, 2026-09-26) · **Design doc:** §3.3, §5, §11.1, I02

## Context

The design requires that runtime decoding, published schemas and portable
validation derive from one registered contract definition (I02), and warns
against Schemars schemas, hand-maintained OpenAPI and UI validation becoming
competing truths. It names Schemars as the reference generator, "an adapter,
not the durable schema format".

M0 had one hand-written decoder (`ObjectReader`) with precise semantics:
400 for wrong JSON types, 422 for missing/null/range/unknown fields, lengths
in Unicode code points, errors in declaration order, and no trimming.

## Decision

Own a small Contract IR (`trestle_protocol::ContractDoc`) and make it the
authority:

- `ContractDoc::validate` is the server's portable validation. It replaced
  `ObjectReader` with the same codes, messages and ordering.
- `json_schema` / `schema_body` generate JSON Schema 2020-12. OpenAPI 3.1.1
  and interaction descriptors embed that output and never define schemas of
  their own.
- `to_ir` + `digest` give a canonical form (sorted keys, sorted `required`)
  for fingerprints and review.
- A Rust type binds to a contract through `impl Contract`. Serde only maps
  the already validated, normalized instance onto the type
  (`deny_unknown_fields`). Registry assembly deserializes each input
  contract's `example()` into its type, so a type/contract mismatch fails
  `pilot check` rather than the first request.
- The M1 field vocabulary: text (code-point bounds), integer (JSON Schema
  integer semantics), boolean, UUID, digit string, and string enum, each
  optional and/or nullable. Anything else is rejected until a renderer and
  validator both support it (§5.4).

Schemars is not used. `jsonschema` (0.58, no network resolver) is a
**test-only** independent oracle.

## Evidence

- `fixtures/contracts/lessons.create.input.json`: 20 accept/reject cases,
  including 200 astral code points, combining sequences, integral floats,
  64-bit overflow, ownership fields and `__proto__`. `examples/pilot/tests/conformance.rs`
  shows the published JSON Schema (via `jsonschema`), the managed route and a
  hand-written Axum handler agree on every case. Changing `max_chars` from
  200 to 199 in the contract makes the test fail, so the check is not vacuous.
- Outputs of every accepted case validate against the published `lessons.view` schema.
- `openapi-spec-validator` accepts `examples/pilot/published/openapi.json`
  (run manually, 2026-09-26).
- The M0 API tests pass unmodified on the new decoder.

## Consequences

- Adding a field type means extending the evaluator, schema generation and
  fixtures together. That is the intended friction.
- There is no derive yet. Contracts are written with a builder next to the
  type. A derive (M3+) must generate the same `ContractDoc`, not replace it.
- Schema output is stable across dependency upgrades because we own it.
  Snapshots in `examples/pilot/published/` make any change a reviewable diff.

## Alternatives rejected

- **Schemars as the source of truth:** its output shape may change between
  releases (§11.1), it infers from serde attributes instead of declared
  rules, and it would still need a separate validator whose semantics
  (code points, unknown fields, 400 vs 422) we would have to match.
- **Validate with `jsonschema` at runtime:** it gives no 400/422 split and no
  stable codes or messages, and it becomes the authority by accident.
