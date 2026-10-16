# ADR-010: Production scope and release approval

**Status:** Proposed (M5, 2026-09-26). It becomes accepted only when the project owner
approves the pilot. · **Design doc:** §16.3, §18 M5, §19.3,
§20.2

## Context

M5 ends with an explicit decision to run a limited pilot. The design doc
asks this ADR to rest on benchmarks, review findings and a rollback and
restore drill, and forbids claiming general maturity, benchmark leadership
or readiness to migrate an established product.

## Proposed decision

Run a limited pilot:

- one product (schools, lessons, units, admin for owners) on one origin;
- OpenID Connect browser sessions;
- PostgreSQL over TLS;
- two or more replicas;
- no durable side effects;

subject to the sign-off checklist in that document.

**Release rule for the pilot.** A release ships only when:

- the gate in `docs/runbooks/deploy.md` §1 is green;
- the compatibility demonstration passes for any schema or form change;
- the change is expand-only, or is a separately reviewed contract step
  after the compatibility window.

## Evidence

### Benchmarks

`docs/evidence/m5-load.json` records the final run: medians of three
rounds, with a prewarm and alternating targets.

| Scenario | Concurrency | p50 ms (Trestle / baseline) | p95 ms (Trestle / baseline) | Throughput req/s (Trestle / baseline) | p95 overhead | Within 20% |
|---|---|---|---|---|---|---|
| retrieve | 8 | 0.79 / 0.76 | 1.24 / 1.28 | 9,237 / 9,506 | -3.7% | yes |
| retrieve | 32 | 3.11 / 2.83 | 4.85 / 4.75 | 8,967 / 9,679 | +2.1% | yes |
| list (page of 20) | 8 | 0.88 / 0.91 | 1.44 / 1.48 | 8,289 / 7,796 | -2.5% | yes |
| list (page of 20) | 32 | 3.56 / 3.51 | 7.13 / 5.95 | 7,528 / 7,915 | +19.9% | yes |
| create (transaction + audit) | 8 | 1.63 / 1.52 | 3.05 / 2.54 | 4,158 / 4,584 | +20.4% | **no** |
| create (transaction + audit) | 32 | 6.05 / 6.23 | 10.23 / 9.55 | 4,552 / 4,566 | +7.1% | yes |

- Five of six cells are within the provisional target of ≤ 20% p95
  overhead. Create at c=8 is at +20.4%, consistently across rounds.
- The machine was under load from other applications (load average about
  15, recorded in the evidence).
- `m5-load-initial.json` is the earlier run: no prewarm, with the locks
  still present. Its get at c=8 cell was +36.7%, mostly a cold start.
- The target is a goal, not a guarantee. The create path should be
  profiled on a quiet host before anyone relies on the margin.

### Review findings

`docs/security-review.md` (self-review) lists controls, proofs and seven
residual risks. External review has not happened and is a sign-off item.
The review changed the code before this ADR:

- The hot path held three process-wide locks: the metrics registry (with
  label allocation per update), the rate-bucket map, and `Uuid::now_v7`'s
  shared context, used for request IDs. They were replaced by atomics under
  a read lock, 16 shards and a context-free v7 respectively.
- `#![forbid(unsafe_code)]` was added to every crate.
- Authentication and CSRF failures now have a bounded metric
  (`trestle_auth_failures_total{reason}`).

### Rollback and restore

- **Restore:** `docs/evidence/m5-restore.json`. The backup restored into a
  new database matched the pre-incident state exactly. The schema verified
  against history with nothing pending, and a second replica served it.
- **Forward repair:** in one audited transaction; every post-incident write
  kept; the one row edited after the incident left for review; the other
  tenant untouched; pre-repair ETags now get 412.
- **Code rollback:** `docs/evidence/m5-demo.json` shows the M4 binary
  serving correctly on the M5 (expanded) schema.
- **Database TLS:** `docs/evidence/m5-db-tls.json`.

## Not claimed

- General framework maturity.
- Performance leadership over anything. The comparison is against a
  hand-written baseline on one laptop.
- Readiness to migrate an established, larger product.

## Consequences

- Until approval, Trestle stays a pilot candidate. Work continues on the
  sign-off items (provider, external review, edge validation, hosting,
  alerts), not on new framework surface.
- If the pilot hits a stop condition from the readiness document, it is
  paused and this ADR is revisited.
