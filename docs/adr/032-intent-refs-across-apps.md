# ADR-032: A `trestle.toml` ref may name another app's model

**Status:** Proposed (2026-10-18) · **Amends:** ADR-024 ·
**Depends on:** ADR-014, ADR-024, ADR-026

## Context

The intent puts each model in the app its ID names (`crm.account` is in
`crm`). A `ref` may name any model in the file (`intent/spec.rs`), and
nothing said it had to be in the same app. But the app generated for a
namespace required only the tenancy app (`intent/runtime.rs`). So a ref to
another app's model, and the relation picker a generated form builds for it,
failed at assembly:

```text
sales.deal.account references crm.account, owned by crm, which app sales does not require
form sales.deal.create.form: relation source crm.account.list is owned by crm, which app sales does not require
```

Every existing test used refs within one app. Any project that splits its
models into apps hits this; Proseware found it with one app per module.

## Decision

- **An intent app requires the apps its refs point to.** For each `ref`
  field on one of its models whose target is in another app, the generated
  app calls `requires(<that app>)`. Assembly orders the apps, and their
  migrations, accordingly; tenancy checks on writes are unchanged
  (ADR-024).
- **Refs between apps go one way.** If the apps' refs form a circle (`crm`
  refers to `sales` and `sales` to `crm`), parsing the file fails with the
  circle named:

  ```text
  trestle.toml: apps refer to each other's models in a circle (crm → sales → crm); refs between apps go one way
  ```

  Without this check the registry would report a generic dependency cycle.
  Refs within one app may still point either way.

## Evidence

`crates/trestle/tests/intent_relations.rs`:

- **Offline:** with deals in `sales` and accounts in `crm`, the project
  assembles; the deal's `account` column references `crm.account`; `crm`
  comes before `sales`, and `sales` requires `crm`; the deal's create form
  (with its relation picker) is published.
- **Circle:** adding a ref from `crm.account` back to `sales.deal` is a parse
  problem naming the circle.
- **PostgreSQL:** a deal in `sales` may name an account of the caller's
  tenant; another tenant's account is `422 not_found` at `/account`, as
  within one app.

Removing the `requires` call fails the offline and PostgreSQL tests.

## Consequences

- Projects can split models into apps by domain, as Django projects do,
  with one-way refs between them.
- Circular refs between apps need the models in one app. If that becomes
  a real limit, it needs its own decision (migrations would have to add the
  foreign key in a later step).
