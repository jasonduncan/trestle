# ADR-001: A thin owned framework, not an adopted larger one

**Status:** Accepted for the pilot (M5, 2026-09-26) · **Design doc:** §1.4,
§17.3, §20.1–§20.2

## Context

The design doc asks whether to own a thin framework over Axum, Tower and
SQLx, or to adopt or extend a larger Rust framework (Cot and Loco are named
as references). The evidence it asks for is a product slice plus an
assessment of the invariant gaps. Five milestones in, the product slice
exists, so the gap can be judged from code rather than from expectations.

## What the pilot needed from a framework

These are the invariants the pilot leans on (§2), with where they live:

| Need | Owned implementation |
|---|---|
| One contract IR as the sole authority for validation, JSON Schema, OpenAPI, descriptors and the TypeScript validator (I02) | `trestle-protocol` (~1.9k lines) |
| Operations with mandatory policies; the executor owns the transaction, admission, recheck and commit classification (I05–I07) | `trestle-core::operation`, `db` |
| Registry assembled offline: apps, dependencies, route inventory with waivers, resources, admin profile (I01, I09) | `trestle-core::registry` |
| Interaction protocol with revisions, retained revisions and an append-only revision log (I03, I11) | `trestle-protocol::interaction`, `registry::meta` |
| Historical migrations: stable model and field IDs, replay without current Rust types, data steps, catalog verification (I12–I14) | `trestle-historical` (~3.3k lines) |
| Identity as an integration (OIDC, sessions, CSRF), authorization in policies | `trestle-identity` (~1.6k lines) |
| Perimeter: admission, rate limits, headers, metrics | `trestle-core::{admission, http, metrics}` |

The owned Rust framework is roughly 12k lines, excluding tests. The React
renderer is about 1.3k lines of TypeScript. Everything beneath is mainstream:
Axum 0.8, Tower, Tokio, SQLx 0.8 and PostgreSQL 16.

## Decision

Keep the thin owned framework for the pilot. Axum stays visible: routes are
ordinary Axum routes, hand-written handlers (`export.csv`) call the same
executor, and an application can drop to raw Axum with a recorded waiver
(ADR-008).

## Rationale

- The distinctive parts are the interaction protocol, the single contract
  authority, retained revisions and historical migration replay. None of
  these appeared in the reference frameworks' documentation (§20.1 makes no
  claim that they do). Adopting a larger framework would still leave all of
  them to build, now against someone else's abstractions.
- The owned layer is small enough to review. That matters because §12.4
  requires external review of the policy boundaries, the session and CSRF
  code, the metadata resolver and the state-changing operations.
- Measured cost: the matched-work load comparison is in
  `docs/evidence/m5-load.json`, and ADR-010 records the result against the
  provisional target.

## Not done: a spike of Cot or Loco

No spike of either framework was run. This ADR decides from the gap
assessment above, not from a side-by-side trial. Revisit if:

- the owned layer grows faster than the product needs it to;
- a reference framework gains a contract IR, or a migration history that
  replays without current types;
- maintaining the historical SDK or the renderer protocol costs more than
  the pilot's value justifies.

## Consequences

- Trestle owns its correctness in areas that are easy to get subtly wrong:
  the planner, replay, cursor signing, sessions and CSRF. The conformance
  tests and drills in `docs/evidence/` are the defence, and external review
  is still required.
- Upgrades of Axum, SQLx and tower-http stay deliberate, tested changes. The
  majors are pinned in the workspace manifest.
