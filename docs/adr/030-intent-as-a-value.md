# ADR-030: The intent as a value

**Status:** Accepted (2026-10-22; proposed 2026-10-18) ·
**Design doc:** §3.4, §11 · **Amends:** the intent API ·
**Depends on:** ADR-014

## Context

A project gets its `trestle.toml` from a file or from text
(`Project::intent_file`, `Project::intent_str`). Code that builds an intent,
such as a test, a scaffold, a generator or per-deployment configuration,
has to write TOML text with string templates. The file's schema is private
(`intent/spec.rs`), and the validated `Intent` can't be written back out. So
there is no typed way to build one, and no way to check a built intent
except by rendering text by hand.

## Decision

- **`trestle::intent::IntentDoc`** is the file's schema as public data:
  - `ProjectDoc`, `TenancyDoc`, `ModelDoc`, `FieldDoc`, `EndpointDoc` and
    `AdminDoc`, plus the existing `AuthSection`;
  - key for key the same as the file, with the same names, defaults and
    `deny_unknown_fields`;
  - it derives `Serialize`, `Deserialize`, `Default` and `PartialEq`;
  - values the file leaves untyped (`default`, `set`, `when`) are
    `toml::Value`, and `trestle::intent::toml` re-exports the crate.
- **One validator.** `IntentDoc` has no rules of its own:
  - `to_toml()` renders the canonical file, leaving defaults out;
  - `validate(name)` runs `intent::parse` on that rendering;
  - so a document and its rendering are accepted, refused and assembled
    identically, and problem lines refer to the rendering;
  - `from_toml(name, text)` reads the shape only.
- **`Project::intent_doc(name, doc)`** uses a document as the project's
  intent. Everything that comes with an intent file comes with it:
  - handlers and hooks, auth and the intent migration check at `serve`;
  - the `tenant` command, the test harness's tenancy, and the project name.
- **Drift guard.** The private file schema and `IntentDoc` must change
  together.
  `tests/fixtures/intent_doc/every_key.toml` uses every key. A test reads it
  as a document, renders it back without loss, and compares everything a
  client or the database can see: operations, schema, contracts,
  interactions, OpenAPI and admin. Assembling from the document must match
  assembling from the file. A key added to the file but not the document
  fails that test.

## Consequences

- An intent can be built in code and checked by the same rules as the
  file.
- There is still one intent per project, and `Intent` stays the validated
  form the runtime uses.
- A new intent key now means editing two structs. The fixture test makes a
  missed one fail.
- Error messages for a document name lines in `to_toml()`. A caller that
  wants them shown in context prints that rendering.
