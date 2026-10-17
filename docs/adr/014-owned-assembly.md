# ADR-014: The registry owns what it assembles

**Status:** Proposed (2026-10-17) · **Design doc:** §3.2, §3.4, §4, I09 · **Amends:**
ADR-003 (`DynOperation`)

## Context

Registration took `&'static` data everywhere: app IDs, `requires`, operation
descriptors (`OperationInfo`), policy IDs, contract references, resource IDs
and paths, and the project name. Rust apps meet that with literals and
`static` contracts. An application assembled from runtime data cannot, so the
`trestle.toml` runtime met it by leaking:

- `Box::leak` on every operation ID, summary, contract and page contract, and
  on the whole parsed intent, so operations could hold `&'static` references
  into it;
- `IntentOp::policy_id` leaked a new string on **every call**;
- `Project` leaked its default name and its custom command names.

That is affordable once per process, but assembly is not once per process:
`check`, tests, `TestApp`, and anything that assembles more than one
application in a process (a hosting layer, a code generator) assemble
repeatedly. Measured on the notes intent, each assembly leaked about 16.7 KB
(837,050 bytes over 50 assemblies).

## Decision

- **Typed apps keep their API.**
  - `Operation::INFO` stays a `const OperationInfo` with `&'static` fields.
  - `Policy::id` and `Contract::contract()` stay `&'static`.
  - `App::id` relaxes to `-> &str`. An impl returning `&'static str` still
    satisfies it.
- **The registry owns its data.**
  - `RegisteredOperation` holds an `OperationDescriptor` (owned `Arc<str>`
    fields; `From<OperationInfo>`), an `Arc<str>` app and policy ID, and
    `Arc<ContractDoc>` input and output.
  - `AppBuilder::requires` and `AppBuilder::resource(id, path)` accept
    anything `Into<Arc<str>>`.
  - `RegisteredResource`, `AppInfo`, `RouteKind` and the `/_meta` index
    hold `Arc<str>`.
  - `Project::new`, `Project::named` and `Registry::name()`/`version()` take
    or return owned or borrowed strings.
- **`DynOperation` borrows from itself:** `info() -> &OperationDescriptor`,
  `policy_id() -> &str`, `input()`/`output() -> &ContractDoc`. Every call
  reads them, so nothing is built per request.
- **The intent runtime shares instead of leaking.**
  - The parsed intent is an `Arc<Resolved>`, and `Intent.models` holds
    `Arc<ModelSpec>`.
  - Operations, rows and handler contexts keep clones of those `Arc`s.
  - Dropping the registry and router frees the intent.
- **Still `'static`, by design:** these are Rust code, never data, so
  literals are what they are:
  - unmanaged route paths and waivers;
  - data-step names;
  - retained form revisions;
  - `ContextDescriptor` header names.

  Out of scope: the CLI's binary name (once per process, a clap
  requirement) and migration files loaded from disk (`trestle-historical`).

## Evidence

- `crates/trestle/tests/assembly_memory.rs` uses a counting global
  allocator. It assembles the notes intent (CRUD, a handler and a hook) and
  builds its router 50 times. Heap growth is **0 bytes**; against the
  previous commit the same test fails at 837,050 bytes (16,741 per assembly).
- The workspace suite passes unchanged apart from tests reading the changed
  field types (`a.id`, `o.info.id`, `ctx.name` are `Arc<str>`).
- Downstream: commons (with the ADR-013 port) and smart-folder build with no
  further changes. commons' admin and auth end-to-end tests and smart-folder's
  tests pass against this change.

## Consequences

- **Breaking, for code that reads registry internals or implements
  `DynOperation`:**
  - `RegisteredOperation`, `AppInfo`, `RouteKind` and `RegisteredResource`
    fields are `Arc<str>` / `Arc<ContractDoc>`;
  - `CommandContext::name` is `Arc<str>`;
  - `DynOperation::info` returns `&OperationDescriptor`.
- Registering a typed operation clones its two contracts once, at assembly.
- A frontend that builds applications from data (the intent runtime today, a
  higher-level layer later) needs no `'static` data and no leaks.
