# ADR-016: Idempotency-Key receipts in the write transaction

**Status:** Accepted (2026-10-22; proposed 2026-10-17) · **Design doc:** §6.4 · **Amends:** ADR-003
(which recorded idempotency keys as not implemented) · **Depends on:**
ADR-013, ADR-015

**Extended by [ADR-033](033-admin-and-application-views.md):** immutable browser
attempts never replay an uncertain write; version expectations are checked before
receipt selection. Retained response bodies must also satisfy the current output
contract before disclosure. A refusal leaves the receipt and effect intact and
does not re-enter execution.

**Extended by [ADR-038](038-idempotent-intent-endpoints.md):** a
`trestle.toml` endpoint opts in with `idempotent = true` (CRUD writes,
actions, non-GET handlers), and the project then installs `IdempotencyApp`
itself.

## Context

§6.4 requires the following.

- An opted-in `Idempotency-Key` is scoped by tenant, principal, operation
  version and key.
- A receipt stores a hash of the canonical input, the result and a retention
  deadline.
- The same key with different input is 409.
- Concurrent duplicates are serialized.
- A replay cannot bypass current disclosure policy.
- Not-started, rolled-back, committed and outcome-unknown stay
  distinguishable; an unknown commit outcome must be resolvable without
  telling clients to repeat blindly.

Until now, every interaction descriptor said `idempotency: none`. Operation
jobs (ADR-015) are at least once, so an attempt repeated after its write
committed would write twice.

## Decision

- **Opt-in per binding.** `Http::post(..).idempotency_key()` makes a write
  accept an optional `Idempotency-Key` header. Assembly refuses it on a read
  operation, and refuses any key-accepting operation unless
  `trestle_core::idempotency::IdempotencyApp` (table
  `trestle_idempotency`) is installed.
- **The receipt lives in the write transaction.** The keyed executor
  (`execute_keyed` / `execute_dyn_keyed`) runs `admit`, then begin,
  `recheck`, and **claim**:
  1. Insert the receipt. Its scope is a SHA-256 of length-prefixed actor,
     tenant, operation ID, operation version and key, with the SHA-256 of the
     canonical validated input, and 24 h retention.
  2. A unique scope makes a concurrent claim wait for the other transaction,
     so duplicates are serialized.
  3. A committed receipt with the same input replays its stored body. A
     different input is 409 `idempotency_key_reused`.
  4. A new claim runs the operation, checks its output, stores the exact
     response body on the receipt, and commits.

  As a result:
  - **Rolled back or failed:** no receipt remains, so a repeat runs again.
  - **Commit outcome unknown:** a repeat either replays or runs, never both.
  - **Replays** happen only after the context is resolved and the policy
    admits and rechecks. A caller who lost access is refused, not answered
    from the receipt.
  - **Expired receipts** are deleted on claim and by
    `idempotency::purge_expired`.
- **HTTP.**
  - A key sent to a route that does not accept one is 400
    `idempotency_unsupported`, so no client believes it is protected. A
    malformed key (not 1–255 visible ASCII characters) is 400
    `idempotency_key_invalid`.
  - A replay returns the stored body byte for byte, with the binding's
    status, `Location` and `ETag`, plus `Idempotency-Replayed: true`.
  - Descriptors of key-accepting operations say
    `{"mode": "key", "header": "Idempotency-Key", "retentionSeconds": 86400}`.
    This is not a required feature, so older renderers keep working;
    descriptors of every other operation are byte-identical.
  - OpenAPI documents the optional header.
- **Direct invocation.** `Invocation::with_idempotency_key(key)` is honored
  by `Registry::invoke`, which knows each operation's opt-in, and refused by
  `operation::invoke`/`invoke_dyn`, which do not.
- **Operation jobs** whose target accepts a key are invoked with the job's
  ID as the key, so their database effects are exactly once even when a
  worker dies after its attempt committed.
- **Metric:** `trestle_idempotency_replays_total{operation}`.

## Evidence

`crates/trestle-core/tests/idempotency.rs` runs on PostgreSQL, over real
HTTP and through `Registry::invoke`:

- a repeat replays the same bytes and `Location`, marked replayed, and writes
  once;
- a different input under the same key is 409 and writes nothing;
- two concurrent requests with one key (a 300 ms write) give one run and one
  replay with equal bodies, and one row;
- an attempt that fails after its insert leaves no receipt, and the repeat
  runs fresh;
- after the caller loses membership, a repeat is 403, not a replay;
- one key from two callers gives two writes;
- a key on a route without opt-in, and malformed keys, are 400 and write
  nothing;
- an expired key starts over, and purge removes it;
- the dynamic-operation route replays;
- `Registry::invoke` replays, refuses keys for operations without opt-in,
  and `operation::invoke` refuses keys;
- an operation job whose first attempt committed before its worker died
  replays on the next attempt: one row;
- assembly refuses a key without the receipt table;
- OpenAPI lists the header only where accepted.

A protocol unit test shows only opted-in descriptors change, with no new
required feature. Mutations are caught: skipping the input-hash comparison
fails the conflict test, and not keying operation jobs fails the
exactly-once job test. The reauthorization test is satisfied by context
resolution, which runs before the executor, so it does not isolate the
`recheck` step. The keyed path still rechecks before claiming, in the same
order as every other write.

## Consequences

- A keyed write holds its transaction while it waits for a concurrent
  duplicate, so lock and statement timeouts bound the wait.
- Only committed successes are stored. Errors are recomputed on repeat,
  because nothing they did persisted.
- The receipt body is the response as first sent. It may contain the same
  data the operation returned, and is kept for 24 h; purge regularly.
- The pilot opts nothing in, so its published documents are unchanged. The
  React renderer does not send keys yet.
