# ADR-012: The executor enforces output contracts before commit

**Status:** Proposed (2026-10-17) · **Design doc:** §5, §6.2, I02, I04, I12 · **Amends:**
ADR-003 (execution sequence)

## Context

Every operation declares an output contract, and that contract is what the
published JSON Schema, OpenAPI document, TypeScript SDK and interaction
descriptors promise to clients (ADR-002). Until now nothing checked results
against it at run time. Only the pilot's conformance tests did, for the cases
they cover. The executor committed a write and then returned whatever the
operation produced (`operation.rs`, `execute_validated` and `execute_dyn` at
0.2.1). So:

- a handler bug could persist a write and then return a body that broke the
  published contract;
- an undeclared field (for example a column the representation should not
  expose) reached the client, even though inputs reject unknown fields;
- a `DynOperation` returns plain JSON and has no Rust type to constrain its
  shape at all.

## Decision

- Both executors validate the result with `ContractDoc::validate` against
  the operation's output contract:
  - **Write:** after `run`, inside the transaction, before `db::commit`. A
    failure returns early, which drops the transaction and rolls it back.
  - **Read:** after `run`, before returning.
- A failure is the operation's fault, not the caller's. It becomes the plain
  500 `internal_error`, with no field errors in the response. The log records
  the operation ID, contract ID and each failing pointer and code, never
  values. `trestle_operation_output_invalid_total{operation}` counts the
  refusals, so the labels stay bounded.
- Typed operations are serialized to a JSON value for the check. The managed
  route still serializes the typed output directly for the response, so field
  order is unchanged. `execute_dyn` returns the normalized value that
  validation produced.
- Every caller gets the check, because every caller goes through these two
  functions (I04): managed routes, hand-written handlers using
  `execute_json`, tests and the intent runtime.
- No postcondition hook is added. A typed or dynamic operation already owns
  its `run` inside the transaction and can check its own invariants there. A
  framework hook would add a second place for the same rule. Revisit this
  when a second caller needs to wrap operations it did not write.

## Evidence

- `crates/trestle-core/tests/executor.rs`:
  - an invalid typed result (a malformed UUID) after an insert leaves no row
    and returns `internal_error` with no field details;
  - an invalid read result is refused;
  - a `DynOperation` that returns an undeclared field after an insert leaves
    no row, and the refusal is counted;
  - valid typed and dynamic writes commit.
  - With the executor change stashed, the three refusal tests fail, and the
    dynamic one leaks the undeclared field to the caller. So the tests are
    not vacuous.
- The workspace suite passes unchanged (270 tests): no pilot, identity or
  intent operation was returning output outside its own contract.
- Load (`scripts/load/compare.sh`): two runs back to back on the same
  laptop, without and with this change
  (`docs/evidence/adr-012-load-{without,with}.json`). The table shows p95
  overhead versus the hand-written Axum baseline in the same run:

  | Cell | Without | With |
  |---|---|---|
  | create c=8 | 3.5% | 2.3% |
  | create c=32 | 2.5% | 2.4% |
  | get c=8 | -0.6% | 4.1% |
  | get c=32 | 0.1% | 3.6% |
  | list c=8 | -5.0% | 10.2% |
  | list c=32 | -4.5% | 6.6% |

  The cost shows where expected: list pages, whose 20 items are serialized
  and validated once more, add about 0.03–0.2 ms at p95. Every cell stays
  within the provisional 20% target. The machine was shared (load average
  7–16), so differences of a few points are noise. These runs needed the
  harness fix in the previous commit: since 0.2.1 the per-address admission
  limit had rejected nearly every load-test request.

## Consequences

- An operation whose output contract is narrower than what it returns now
  fails loudly instead of leaking. Fix the contract or the representation;
  never loosen the check.
- Each operation pays one extra serialization and validation pass. For list
  pages this grows with the number of items. Measure before optimizing:
  a non-allocating check, or reusing the validated value for the response,
  are both possible later without changing behavior.
- Output contract changes (adding a field to a representation) must now land
  together with the code that produces the field. Otherwise the first request
  fails, rather than the client simply receiving an undocumented field.
