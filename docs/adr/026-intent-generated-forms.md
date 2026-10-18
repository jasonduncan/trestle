# ADR-026: Generated forms for `trestle.toml` writes

**Status:** Proposed (2026-10-17) · **Design doc:** §9 · **Amends:** the
intent format · **Depends on:** ADR-022, ADR-024, ADR-025

## Context

Rust apps publish forms (`app.form(FormPresentation…)`) that the React
renderer draws from interaction descriptors. Intent apps had operations
and routes, but no forms, so a `trestle.toml` app had no UI without
writing Rust. Forms were the last parity gap the guide named.

## Decision

- **Opt-in per endpoint: `forms = true`.**
  - On a CRUD endpoint it publishes `<model>.create.form` ("New deal",
    Create) and `<model>.update.form` ("Edit deal", Save) for the ops the
    endpoint has.
  - On an action it publishes `<action id>.form` ("Win deal", Win) over the
    fields the action accepts.
  - Endpoints without it publish nothing new.
- **Fields** appear in declaration order, labelled by `label` or the ID in
  words (`closed_at` → "Closed at"). The widget follows the type:

  | Type | Widget |
  |---|---|
  | `text` | `textarea` when it has no `max` or `max` > 200, otherwise `text` |
  | `int` | `integer` |
  | `bool` | `checkbox` |
  | `enum` | `select` |
  | `decimal` | `decimal` |
  | `date` | `date` |
  | `instant` | `datetime` |
  | `duration` | `duration` |
  | `ref` | `relation`, picking from `<target>.list` and showing the target's `label` field |
  | plain `uuid` | none (left out when optional) |

  Descriptors then require the matching renderer features (ADR-022, I11).
- **Labels for pickers.** A model's `label = "<text field>"` names what
  pickers show for its rows. It defaults to the model's first text field.
- **Checked when the file is read:**
  - a required plain `uuid` field in a generated form: make it a `ref`;
  - a `ref` whose target has no list endpoint or no text label;
  - a model `label` that isn't a text field;
  - `forms` on a handler endpoint.

  Registry assembly still checks every form against its operation, as for
  Rust forms.
- **Not included:** admin views. `app.resource(..)` takes typed operations
  only, and intent operations are runtime-built, so admin views for intent
  models need the resource API extended first.

## Evidence

- **`crates/trestle/tests/intent_forms.rs`** (offline):
  - the create form's widgets, labels (`label = "Value"` included) and
    relation source (`crm.account.list`, `/api/v1/accounts`, `name`);
  - the features it requires;
  - the update form, which requires `http.precondition@1`;
  - the action form;
  - opt-in: no form for an endpoint without `forms`;
  - the four problems above;
  - the create form equals `fixtures/interactions/intent-crm-deal-create.json`,
    regenerated with `TRESTLE_BLESS=1`.
- **`frontend/interaction-react/test/intent-forms.test.ts`:** the renderer
  decodes that descriptor with no missing features, and every field has a
  default widget that accepts it.
- **Mutation checks, each caught:** decimals given a text box; relations
  picking from the wrong operation; forms not opt-in; form fields not
  checked.

## Consequences

- **Forms derive from the file,** so a field change moves the form's
  revision as it would for a Rust form. Open-form skew follows the usual
  `revisions.json` rules for apps that publish snapshots.
- **For layout or help text beyond labels,** write the form in Rust.
