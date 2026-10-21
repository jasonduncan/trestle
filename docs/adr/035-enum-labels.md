# ADR-035: Display labels for enum values

**Status:** Proposed (2026-10-21; design agreed by the project owner) ·
**Design doc:** §5.2, §9, §14 · **Amends:** ADR-026, ADR-033 ·
**Depends on:** ADR-023

## Context

An enum field stores and sends short codes (`contract`, `new`). Forms,
filters and record pages showed those codes as they are. Changing what
people see meant renaming the stored value, which is a data migration.
Django's `choices` pairs each stored value with a display label for this
reason. `FieldDoc.label` names the whole field, not its values. Proseware,
which generates `trestle.toml` files, needs labels it can change freely.

## Decision

- **`trestle.toml`:** an enum field may give
  `labels = { contract = "Agreement", new = "New lead" }`, a table from
  value to label. `values` stays a list of plain strings, so existing files
  and `IntentDoc` documents read as before. A value without a label shows
  as itself. Problems, on the label's line:
  - `labels.<key>: "<key>" is not one of the field's values`;
  - `labels.<key> must be 1 to 200 characters` (code points);
  - `labels = {...} is for enum fields`.
- **`IntentDoc`:** `FieldDoc.labels`, a `BTreeMap<String, String>`, empty by
  default and left out of the rendering when empty.
- **Contract IR:** `FieldType::Enum { values, labels }`, built with
  `Field::enumeration(..).labels(&[("contract", "Agreement")])`. The IR
  writes `"labels"` only when there are some. Contract problems mirror the
  file's: a label for a value that isn't one, or one outside 1–200
  characters.
- **JSON Schema and OpenAPI:** `enum` stays the only rule. The labels go in
  an annotation, `"x-trestle-enum-labels": {"contract": "Agreement"}`, next
  to it. The `x-trestle-` prefix follows `x-trestle-operation-version`.
  Validators ignore annotations, so every client accepts and rejects
  exactly what it did before. The standard alternative, `oneOf` of
  `{const, title}`, was not used: the browser already reads `oneOf` as a
  tagged union (ADR-028), and it would rewrite the validation shape of
  every labelled field, nullable ones included.
- **Values stay values.** The API, the column, filters (`?stage=contract`),
  action `set`/`when`, defaults and `choice` error messages and parameters
  all use the stored value. A label sent as a value is a `choice` error.
- **Never schema.** Labels are not part of schema state (I10). Changing,
  adding or removing them plans no migration and leaves the desired schema
  equal, so `makemigrations --check` stays clean. Changing `values` still
  alters the CHECK constraint.
- **Browser.** `@trestle-framework/client` decodes the annotation into
  `FieldSchema.labels`, refusing a label for a value that isn't a choice
  or one of the wrong length. `choiceLabel(field, value)` returns the
  label or the value. The select widget shows the label and submits the
  value. Admin and application views show labels in columns, record pages,
  related sections and filter options (`formatValue(value, field)`).
- **Same documents without labels.** Nothing new is written when a field
  has no labels: IR, schemas, interaction descriptors and view documents
  are byte-identical to 0.7.1, so 1.0 descriptors stay byte-identical
  (ADR-033). Labels need no renderer feature and no protocol version,
  because a renderer that showed raw values would still submit correctly.

## Evidence

- **`crates/trestle/tests/intent_types.rs`:**
  - `enum_labels_are_presentation_only`: the create input, list filter
    input and representation schemas carry the labels; removing them, or
    changing and adding some, leaves the desired schema equal and the plan
    empty; without labels the field schema is
    `{"type": "string", "enum": [..]}` exactly; an added value does plan a
    change.
  - `crud_keeps_every_value_in_its_contract_form`: creating with
    `"contract"` stores and returns `"contract"`, `?status=contract` finds
    it, and `"Agreement"` is a 422 `choice` at `/status`.
  - `enum_labels_are_checked`: the three problems above, with lines.
- **`intent_forms.rs`:** the generated create form's input and output
  schemas carry `{"won": "Closed won"}`; the fixture
  `fixtures/interactions/intent-crm-deal-create.json` is regenerated.
- **`intent_doc.rs`:** `every_key.toml` uses `labels`, and the document
  round-trips and assembles like its file.
- **Browser:** `interaction-react/test/intent-forms.test.ts` decodes that
  fixture, renders the select, finds the option "Closed won" with value
  `won`, submits `won`, and refuses a label for an unknown value.
  `client/test/views.test.ts` shows a labelled value by its label.
- **Unchanged:** the pilot's published snapshots and the view fixtures,
  which use no labels.

## Consequences

- **Older renderers refuse labelled schemas.** The 0.7.x decoders reject
  unknown schema keywords, so they refuse a descriptor or view that uses
  labels rather than show raw codes. Ship the browser packages with the
  server, as before.
- **A label change is a new revision.** Labels are in the contracts, so
  changing one changes contract digests, interaction and view revisions,
  and the projection that generated cursors bind (ADR-033). Open pages
  refetch and cursors need a fresh first page, as for any representation
  change. No stored data or migration is involved.
- **`FieldType::Enum` has a new member.** Code that matches
  `FieldType::Enum { values }` needs `..`, and code that builds `FieldDoc`
  without `..Default::default()` needs `labels`.
- **One language.** Labels are plain text in one language; translation is
  not part of this decision.
