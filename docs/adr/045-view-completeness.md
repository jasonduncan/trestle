# ADR-045: A collection view that says it is incomplete

**Status:** Proposed (2026-10-23) · **Amends:** ADR-033 ·
**Depends on:** ADR-033

## Context

A collection view (ADR-033) shows one page of a registered read: the rows at
`/items`, and a "Next" button when `/next_cursor` holds a cursor. Some reads
can't page. They compute their rows, cap them at the 100-row page limit, and
say in their output whether they stopped early, usually with a `complete`
boolean. The view document has no place for that flag, and the renderer
ignores it, so a list cut off at 100 rows looks exactly like a list that holds
every row.

Applications that must not present a truncated result as whole need the flag
shown. Proseware's generated reads are one case: its CRM requirements say a
limited result is identified as incomplete (CRM-QUERY-04). A hand-written
report or a search over an external index is another.

## Decision

### `ViewDefinition::complete`

```rust
ViewDefinition::collection("reports.overdue", "reports.overdue.list")
    .complete("complete")
```

- It names a field of the collection's output, beside `items`, that is `true`
  when the page holds every row the read found and `false` when it stopped
  early.
- Assembly refuses the view unless the field is a required, non-null boolean
  at the top of the output, and the view is a collection. A record view has
  no page to be incomplete.

### The view document

- `SourceDoc` gains `completePointer`, the field's pointer (`/complete`). It is
  left out of the document when the view names no flag, so existing documents
  and their revisions are unchanged.
- A view that names a flag adds the required feature `views.completeness@1`.
  A renderer that doesn't support it shows "this view needs a newer
  renderer", per ADR-033, instead of a list that looks whole.
- The protocol version stays `1.0`, because the feature mechanism covers this.

### The renderer

- `@trestle-framework/client` decodes `completePointer`. It must point at a
  required boolean of the output schema.
- The collection view reads the flag on each page. When it is `false`, it
  shows a status line above the table: "This list is incomplete: more rows
  exist that it doesn't show." A `true` or absent flag shows nothing. The line
  doesn't count rows, because a later page of a read that pages and caps
  doesn't hold the first rows.
- Both checks refuse the same documents: a pointer that isn't a required
  boolean of a collection's output, or a pointer without the feature.
- Paging is unchanged. A read may have both a cursor and a flag; the flag
  describes the page it returned.

## Consequences

- **Breaking for struct literals:** `ViewDefinition` and `SourceDoc` gain a
  public field, so code that builds them with literals must add it. The
  builders are unchanged. This goes in a minor release before 1.0.
- **No change to any existing view:** documents without a flag keep their
  bytes and revisions. Renderers that predate the feature keep working for
  them.
- **`trestle.toml` `[[views]]` gets no `complete` key yet.** Generated intent
  lists page with cursors and have no such field. A key can be added when an
  intent read gains one.
- **A record view's related sections carry no flag.** `SectionDefinition` has
  no `complete`, so a section's source never has `completePointer`, and a
  related list over a capped read shows no notice, as before. A section flag can
  be added the same way when one is needed; until then, a capped read belongs
  in its own collection view.
- **Residual:** the flag says that rows are missing, not how many. A read that
  can count them can put the count in a column or title. The view doesn't
  carry one.
