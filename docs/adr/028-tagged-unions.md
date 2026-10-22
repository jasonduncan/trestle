# ADR-028: Tagged unions in contracts

**Status:** Accepted (2026-10-22; proposed 2026-10-17) ·
**Design doc:** §5.2, §5.4, §9.2 · **Amends:** ADR-022, ADR-027

## Context

Some inputs take one of several shapes: a payment by card needs the last
four digits, an invoice needs a due date, and cash needs nothing. Before
this ADR, a contract could only make every member optional and leave the
real rule to code. That rule was invisible to the schema, the browser and
the SDK.

## Decision

- **`Field::union(name, tag, &[(variant, &contract), ..])`.** The tag is a
  member that names the variant: `{"method": "card", "last4": "4242"}`.
  Each variant is a contract for the other members.
  - **Variants are kept in name order,** as the IR stores them, so a built
    contract and its round trip are equal.
  - **Definition problems:** an identifier tag; at least one variant;
    unique identifier variant names; no variant declaring the tag; no list
    or union inside a variant (one level, so pointers and forms stay
    simple).
- **JSON Schema:** `oneOf`, one object branch per variant. Each branch is
  the variant's schema, with the tag added as `{"const": "<variant>"}` and
  required. A nullable union ends with a `{"type": "null"}` branch. The
  `const` values differ, so at most one branch matches.
- **Validation, server and browser:**
  - Not an object: 400 `type` at the field.
  - The tag is checked first: missing is 422 `required` at
    `/payment/method`; not a string (or `null`) is 400 `type`; an unknown
    variant is 422 `choice` with `params.choices`.
  - The chosen variant's contract then checks the other members. Its
    errors keep their codes, with the union's pointer as a prefix
    (`/payment/last4`, `/refund/amount/currency`). A member from another
    variant is `unknown_field`.
  - The normalized value is the variant's, plus the tag.
- **IR:** `"type": "union"`, `"tag"`, and `"variants"` as an object from
  name to contract IR.
- **Compatibility:** adding a variant is compatible. Changing the tag,
  dropping a variant, or narrowing a variant (as for any contract) rejects
  older instances, and the reason names the variant.
- **SDK:** a TypeScript discriminated union on the tag:
  `{ method: "card"; last4: string } | { method: "cash" }`.
- **The browser decoder** accepts `oneOf` only in the exact shape the
  server writes:
  - every object branch pins the same required tag with `const`;
  - a null branch may come only last;
  - names are unique;
  - no union sits inside a variant.

  Any other `oneOf`, and `anyOf`, is refused.
- **Forms.** The `union` widget is a variant select. The chosen variant's
  members appear below it, each with the default widget for its schema.
  - **Feature:** a form using the widget requires `forms.union@1`, and the
    renderer declares it.
  - **Switching variants** keeps what was typed in the others but sends only
    the chosen variant's members.
  - **Labels** for variants and members come from their names ("Last4"),
    because presentation has no nested labels yet. An app can replace the
    widget by key.
  - **uuid members** are plain text inputs, because a member has no option
    source.
- **Nested errors in `OperationForm`:** an error whose pointer is under a
  field (`/price/amount`, `/payment/last4`) is shown on that field and
  prefixed with the member ("Last4: …"). Before this, money's member errors
  only reached the summary.
- **Not in `trestle.toml`.** A union has no column mapping yet. A Rust app
  stores it as it likes (columns per variant, or JSON).

## Evidence

- **`fixtures/contracts/unions.json`, 19 cases.**
  - **Accepted:** each variant, an optional member, a nullable member, an
    empty variant, money inside a variant, and a null optional union.
  - **Errors:** no union; no tag; an unknown, numeric or null tag; a
    non-object; another variant's member; a member breaking its rule;
    a missing member; an impossible date; a member of the wrong type;
    money errors three levels deep; and shape errors winning over invalid
    ones.

  The server (status, codes, order, normalized value), `jsonschema` with
  formats, and the browser agree on every case, in the same error order.
- **Contract tests:** round trip, a valid example, name order, each
  definition problem, and each compatibility reason.
- **Browser decoder:** refuses an untagged branch, an optional tag,
  duplicate variants, a nested union, a loose branch, a leading null branch,
  and `anyOf`.
- **Widget tests:** only the chosen variant's members are sent, a lone
  variant is preselected, null clears it, and an edit round-trips.
- **`OperationForm` test:** fields switch by variant, a local member error
  shows as "Payment: Last4: …" and marks the field, the submitted body
  carries the tag, and a server error at `/payment/last4` shows on the
  field.
- **Mutation checks, all 20 caught (each failed a test, none failed to
  compile):**
  - **Server:** unprefixed pointers; a missing tag as a shape error; the
    tag not required in the schema; no null branch; a dropped variant or a
    changed tag treated as compatible; a variant allowed to declare the
    tag; nested unions allowed; the tag lost from the normalized value; an
    unknown tag validated as the first variant; the SDK dropping the tag;
    the widget needing no feature.
  - **Browser:** unprefixed pointers; the tag not required; the null branch
    ignored; nested unions decoded; members of every variant sent; the
    first variant's fields always shown; nested errors kept off the field;
    the renderer not declaring `forms.union@1`.

## Consequences

- **The portable profile has two composite field types,** money and unions.
  Lists in inputs and nested unions stay out.
- **Variant order is alphabetical** in schemas and pickers. Display order
  and labels need presentation support, which can come later without a wire
  change.
- **Older renderers** that lack `forms.union@1` refuse to submit such forms,
  as I11 requires, rather than guessing.
