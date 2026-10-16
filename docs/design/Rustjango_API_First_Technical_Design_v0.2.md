# Rustjango
## API-First Application Framework
### Technical design and architect handoff · v0.2

**Date:** September 25, 2026  
**Status:** Proposed design; not an implemented or benchmarked framework  
**Audience:** Software architect, framework engineers, frontend engineer, security reviewer  
**Context:** Product-led development by an experienced Django/DRF engineer, with incremental Rust implementation  
**Supersedes:** Rustjango Technical Architecture and Design Specification v0.1 for product scope and interface architecture  
**Naming:** Rustjango and all package names are working names, not cleared or published names.

> Django-style application composition. Typed application APIs. Backend-described interactions. Independently delivered clients.

# Document map

The core decision is in §§1–3. Application composition and execution contracts are in §§4–7. The new interaction protocol, generated forms, administration, and compatibility model are in §§8–12. Persistence and historical migrations remain in §§13–14. Runtime, delivery, development, milestones, and acceptance criteria are in §§15–20. Appendices provide implementation examples, a metadata specimen, and primary sources.

**How to read the requirements.** MUST and MUST NOT define this proposal's required behavior. SHOULD identifies a default that can change through a documented architecture decision. Every `rustjango` API, CLI command, package, protocol field, and illustrative Rust/TypeScript snippet is proposed. Examples are interface sketches, not tested library APIs. Performance and productivity targets require measurement.

**Scope boundary.** The core never needs to render application HTML. React forms and admin are optional clients. This does not remove backend validation, authorization, state transitions, or the need to maintain a browser protocol.

**What changed from v0.1**

| Previous direction | v0.2 decision |
|---|---|
| Broad Django replacement, including templates and HTML forms | API-first application framework; no template engine or server-rendered form subsystem |
| Server-rendered administration in the initial framework | Optional admin backend extension and separate React client |
| Presentation mostly part of an internal resource description | Explicit, versioned interaction protocol with audience-scoped publication |
| Prove schema machinery before the integrated UI experience | Prove a protected operation and runtime-rendered form early; evolve persistence automation alongside it |
| Framework adoption as the initial default gate | Own a small integration layer for the product; evaluate existing components against concrete gaps |
| API-first mostly a transport preference | Typed contracts, operations, and independent frontend deployment shape the architecture |

The previous work on stable identities, historical migrations, policy-scoped queries, transaction outcomes, and recoverability remains applicable. This revision restates the required foundations so an architect can work from this document alone. [P1]

# 1. Product definition and boundaries

## 1.1 The system we are building

Build a modular Rust framework for application APIs with the integrated development experience the product owner values in Django and Django REST Framework. Its distinguishing integration is a backend-owned description of application interactions: accepted inputs, returned representations, exposed operations, and optional presentation hints.

The product frontend remains a normal React application. It may use generated TypeScript clients, runtime-generated forms, entirely custom screens, or a mixture. The backend is useful without any supplied frontend package. The form renderer is useful without the admin. Neither requires server-side HTML generation.

The objective is not to reproduce Django syntax, claim universal superiority over Django, or win an empty-endpoint benchmark. It is to reduce the amount of application plumbing required to expose a well-defined, authorized operation and make it usable from clients.

## 1.2 Four domain primitives, two integration boundaries

| Primitive | Owns | Does not own |
|---|---|---|
| **Model** | Desired persistence shape, stable field identities, constraints and relationships | Public request/response exposure |
| **Contract** | Wire representation, structural constraints, stable validation codes | Database layout or caller permission |
| **Operation** | Application behavior, policy binding, transaction and outcome semantics | A particular React component |
| **Presentation** | Labels, grouping, widget intent, display and interaction hints | Authorization or executable business logic |
| **App** | Composition and ownership of a cohesive application feature | Isolation from malicious trusted code |
| **Resource** | A selected API-facing view of queries, operations, and contracts | An automatic endpoint for every database table |

Policies are mandatory bindings on exposed capabilities, not optional UI annotations. Queries are read operations with collection-scoping requirements. A resource may combine multiple models, expose a computation, or have no persistence model at all.

## 1.3 Initial delivery profile

The first product pilot uses stable Rust, Tokio, Axum/Tower, PostgreSQL, explicit app installation, JSON APIs, one database, and independently deployed React assets. The supported scalar and query surface is intentionally narrow. Authentication, tenant isolation, input/output separation, and migration replay are required from the first protected data slice.

Generated schemas and a small form renderer are early proof points. A reusable CRUD/resource layer and admin follow proven operations. Background jobs, uploads, streaming, and richer form widgets are added only when required by a demonstrated product workflow.

**Out of scope:** templates, SSR, a general frontend build service, Django source compatibility, a full replacement for every Django package, runtime native plugins, a visual low-code builder, arbitrary executable UI expressions, automatic production schema synchronization.

## 1.4 Relationship to existing tools

FastAPI already demonstrates typed request/response contracts, generated OpenAPI, and client-generation opportunities. DRF already provides configurable resource metadata, including field information through OPTIONS. JSON Forms already separates data schemas from UI schemas. The opportunity here is a deliberate integration of these ideas with Rust application modules, operations, policy enforcement, and schema history, not the invention of metadata-driven forms. [R1, R2, R3]

# 2. Architectural invariants

These invariants are acceptance conditions, not aspirational descriptions.

| ID | Requirement |
|---|---|
| I01 | Installing a model MUST NOT publish an API, form, admin resource, or writable field. |
| I02 | Runtime decoding, published schemas, and portable validation MUST derive from the same registered contract definition. |
| I03 | A metadata document, enabled button, hidden field, or client check MUST NOT confer authority. Execution rechecks policy. |
| I04 | Generated API, form, admin, and job paths MUST call the same authorized operation entry point for the same behavior. |
| I05 | Tenant/row scope MUST constrain queries before pagination, counts, relationship choices, or disclosure. |
| I06 | Field access, JSON serialization, and form rendering MUST NOT trigger database I/O. |
| I07 | Desired schema, committed historical schema, and observed database state MUST remain distinct. |
| I08 | Historical migrations MUST survive changes to current Rust model types and current business logic. |
| I09 | App registration and export MUST be deterministic and usable offline without startup side effects. |
| I10 | Presentation changes MUST NOT generate database migrations; schema identities MUST survive Rust refactoring. |
| I11 | Old browser bundles and open forms MUST encounter explicit compatibility behavior, not silent reinterpretation. |
| I12 | A timeout, task spawn, or transaction MUST NOT be presented as stronger durability than it provides. |
| I13 | Generated conveniences MUST leave ordinary Axum handlers and explicit application code available. |
| I14 | Security and performance MUST be demonstrated through application-level tests and measurements. |

The Rust type system can constrain the interfaces we expose. It does not establish that a deployed database matches the schema, a policy is correct, an external effect did not happen, or a running browser understands a newly introduced widget.

# 3. System architecture

## 3.1 Component view

```text
APPLICATION SOURCE
  Apps | Models | Contracts | Operations + Policies | Presentation
                            |
          Small local derives + explicit registration
                            |
               Validated immutable registry
                  /          |          \
          Schema IR     Contract IR    Interaction IR
              |              |              |
      Migration tooling   JSON Schema    Audience projection
              |           OpenAPI/SDK        |
              |              |         Metadata API
              |              |              |
              |         HTTP adapters        +---- React forms
              |              |              +---- React admin
              |              |              +---- Custom clients
              |              v
              |    Authorized operation executor
              |       |               |
              |   Scoped queries   Transaction + audit + outbox
              |       |               |
              +-------+---------------+
                            |
                  SQLx / PostgreSQL

FRONTEND DELIVERY: React build -> CDN -> browser
API DELIVERY:     browser -> edge /api and /_meta -> Rust service
```

**IR** means intermediate representation: a versioned description consumed by another component. Nothing here requires an AI model, agent runtime, or semantic inference service.

## 3.2 Three phases, not one giant macro

**Build:** local derives generate adapters and type-local descriptors. They rely on ordinary trait implementations for referenced types. Procedural macros operate on token streams; they are not a whole-program reflection system. Cross-app validation belongs in registry assembly. [R4]

**Assembly:** the project collects installed apps, resolves IDs, checks dependencies and exposure, produces canonical schema/contract/interaction representations, and freezes the registry. An offline command runs this same pure path. Production startup subsequently initializes services and verifies release compatibility before admitting requests.

**Execution:** an HTTP adapter establishes identity and trusted context, decodes and validates input, and invokes an operation. The executor applies policy, handles concurrency and transactions, and returns an explicit output or error. Metadata is an audience-scoped projection of the registry; it is not the execution engine.

## 3.3 Reference stack and ownership

| Layer | Reference choice | Framework responsibility |
|---|---|---|
| HTTP and async | Tokio, Axum, Tower | Lifecycle, request context, operation adapters, defaults |
| Serialization | Serde and serde_json | Explicit input/output contracts and decoding policy |
| Contract schema | Schemars adapter with pinned dialect | Canonical Contract IR, portable rules, conformance tests |
| Database | SQLx and PostgreSQL | Scoped repositories and transaction ownership |
| Dynamic query construction | SeaQuery when justified | Allowlisted query semantics; bounded typed facade |
| OpenAPI | Registry exporter; evaluate utoipa as a builder/adapter | One route/contract source, no independent schema authority |
| Forms | Small React adapter; evaluate JSON Forms | Interaction protocol, widget registry, transport, compatibility |
| Identity/crypto | Existing reviewed providers and libraries | Actor abstraction, safe configuration, policy integration |

Axum supplies the HTTP foundation and integrates with Tower/Tokio. SQLx supplies execution/pooling; SeaQuery supplies query construction. Schemars generates JSON Schema from Rust types, while utoipa provides OpenAPI tooling. Their existence does not prove our exact adapters or interoperability profile. Pin releases and verify them in the implementation spike. [R5, R6, R7, R8, R9]

Do not keep Schemars schemas, hand-maintained OpenAPI schemas, and separate UI validation definitions as competing truths. Structural type metadata and declared portable rules lower into one normalized Contract IR, and all exports consume it.

## 3.4 Registry representation

An in-process descriptor can contain function adapters and static references. A portable IR MUST NOT contain closures, Rust TypeId values, memory addresses, secrets, or compiler-generated paths as durable identity.

Maintain separate fingerprints for persistence, wire contracts, and presentation. Canonicalization sorts unordered collections, preserves intentionally ordered UI elements, and excludes build timestamps and source-machine paths. A public projection has its own digest, not the digest of a private registry. A hash identifies a representation under these rules; it does not prove arbitrary business semantics are equivalent.

# 4. Django-style application composition

## 4.1 Installation and ownership

An app is a feature boundary: accounts, lessons, billing, or another product capability. It contributes models, contracts, operations, resources, migrations, and optional interaction metadata. Apps can begin as Rust modules and become reusable crates without changing the composition model.

```rust
// Proposed application-facing API.
impl App for LessonsApp {
    fn id(&self) -> AppId { AppId::new("lessons") }

    fn register(&self, app: &mut AppBuilder) -> Result<()> {
        app.requires("identity", "1")?;
        app.requires("schools", "1")?;
        app.model::<Lesson>()?;
        app.operation::<CreateLessonOp>()?;
        app.operation::<PublishLessonOp>()?;
        app.resource(lesson_resource())?;
        app.presentation(lesson_forms())?;
        app.migrations(lessons_migrations::registry())?;
        Ok(())
    }
}

Project::new()
    .install(IdentityApp::configured())
    .install(SchoolsApp)
    .install(LessonsApp)
    .serve(settings)
    .await?;
```

Registration is pure. It does not connect to PostgreSQL, load user-specific permissions, spawn tasks, or send external requests. A heterogeneous registry uses a dyn-compatible adapter; generic convenience methods and associated constants must not accidentally make the runtime app interface non-dispatchable. Async lifecycle hooks use an explicit boxed-future or equivalent adapter. [R10]

## 4.2 Isolation and dependency rules

Each app owns a stable namespace and its persisted objects. Other apps use published operations, query interfaces, contracts, or declared extension points. Shared data access is allowed only through an explicit dependency and reviewed interface. Apps cannot silently mutate another app's schema or replace its policy.

Lifecycle dependencies form a DAG and are validated before boot. Model relationships may be cyclic; schema construction handles their foreign keys separately. Migration dependencies are a third graph. None of these graphs eliminates Rust's own crate dependency constraints.

A reusable extension may contribute its own join table, resource overlay, widget intent, or registered subscriber. Duplicate overlays and competing operation IDs fail assembly unless an explicit replacement rule is configured.

These are trusted compiled-in modules, not a sandbox. Removing an app from runtime installation does not mean its production tables may be dropped. Decommissioning persisted features requires an explicit migration plan, reference checks, and data retention decision.

## 4.3 Lifecycle

Startup order is registration, offline validation, configuration validation, service initialization in dependency order, readiness checks, then admission. Failed initialization unwinds initialized services in reverse order. Shutdown stops new requests and job claims, drains bounded work, cancels supervised tasks, then closes services in reverse order.

Service injection uses explicit typed handles. The public request context does not expose a global unrestricted database pool by default. Runtime configuration may alter approved operational settings, but cannot silently change the persistence app set or metadata identity.

# 5. Contracts, validation, and wire semantics

## 5.1 Separate database and boundary types

Use distinct model, creation input, patch input, response, and operation-input types. A database field's existence is not a reason to accept it from a client. For the example, school ownership, internal flags, and publication timestamps are derived or changed by operations, not copied from arbitrary request bodies.

```rust
// Proposed derives. Presentation is registered separately.
#[derive(ApiInput)]
#[input(id = "lessons.create.input", version = "1.0.0")]
pub struct CreateLessonInput {
    #[field(min_length = 1, max_length = 200)]
    pub title: String,
    #[field(minimum = 1, maximum = 12)]
    pub grade_level: u8,
}

#[derive(ApiOutput)]
#[output(id = "lessons.view", version = "1.0.0")]
pub struct LessonView {
    pub id: Uuid,
    pub title: String,
    pub grade_level: u8,
    pub status: LessonStatus,
    pub version: VersionToken,
}
```

The input adapter rejects unknown fields by default. Output projection selects fields explicitly and cannot serialize hidden model fields or relation caches by accident. Supporting a custom Serde implementation requires a schema/codec adapter and conformance fixtures; a derive cannot infer arbitrary serialization behavior.

## 5.2 One constraint definition, several consumers

Portable constraints form a bounded rule representation: requiredness, nullability, length, numeric bounds, enums, and supported format checks. The server evaluator and JSON Schema export consume those same rule nodes. Type derivation supplies structure; registered rules supply additional semantics.

Database-backed and business rules remain server-only: uniqueness under concurrency, school membership, whether a lesson can be published, or whether a billing plan allows an operation. Metadata may identify safe validation codes and advisory requirements, but MUST NOT serialize arbitrary Rust predicates or claim that the browser can reproduce them.

Client validation is feedback, not enforcement. Server validation always runs, followed by business rules and database constraints. A successful asynchronous validation request is not a reservation or guarantee that a later write succeeds.

## 5.3 Requiredness, null, defaults, and PATCH

| Input condition | Contract meaning |
|---|---|
| Required property missing | Invalid input |
| Optional property missing on create | Apply explicitly declared omission behavior, not an inferred UI default |
| Property present as null | Accepted only when null is part of the wire contract |
| Patch property absent | Leave the stored value unchanged |
| Patch property null | Clear a nullable value; reject for a non-nullable field |
| Patch property with a value | Replace that field, subject to validation and policy |

Use a custom presence type for patching, conceptually `Missing | Null | Value(T)`, with explicit decoding and schema generation. A plain Option is not the specification for a three-state patch. Arrays replace as a unit in the initial patch profile; recursive merge and arbitrary JSON Patch operations are deferred. The API advertises this documented partial-object format as application/json, not as an unimplemented standards media type.

UI initial values, server omission defaults, database defaults, and one-time migration backfills are separate. JSON Schema default is an annotation and does not itself fill missing values during validation. Its readOnly/writeOnly annotations also do not substitute for server input/output enforcement. [R11]

## 5.4 Cross-language types and limits

Publish an explicit codec table. UUIDs and date/time values are strings; timestamps use a documented RFC 3339 profile. Unbounded 64-bit IDs, exact decimals, and version tokens use strings unless a safe numerical range is part of the contract. No implicit JavaScript floating-point conversion is allowed for exact values.

Define string length in Unicode code points and test astral characters; normalization or trimming is explicit, not a surprise performed by one client. Portable regex support is restricted to a reviewed common subset with bounded input; arbitrary engine-specific expressions remain server-only. Format annotations such as email need a named validator profile because JSON Schema format is not automatically asserted by every implementation. [R12]

The initial auto-form subset covers objects, strings, booleans, bounded integers, string enums, nullable scalar values, finite arrays, and local references. Arbitrary unions, recursive forms, dynamic maps, and custom wire codecs require explicit renderer support or are rejected for auto-form publication. They may still be valid custom API contracts.

# 6. Operations and the execution boundary

## 6.1 The application behavior contract

An operation registers a stable ID, input/output contracts, security audience, policy binding, execution handler, transaction mode, outcome types, audit policy, and optional idempotency behavior. HTTP binding and presentation are separately registered references to that operation.

A query operation additionally declares its filter/sort/page contracts and collection scope. A command declares its state transitions and concurrency preconditions. Not every internal helper becomes an operation: use the boundary for exposed actions and behavior reused across transports.

A custom Rust handler remains ordinary application code. Generated adapters must call the executor rather than directly invoking an internal unchecked function. Where practical, private modules and executor-issued contexts make accidental bypass difficult. This is not protection against trusted application code intentionally opening another database connection.

## 6.2 Command execution sequence

```text
HTTP admission and body limits
  -> authenticate and resolve trusted actor/tenant
  -> decode and portable validation
  -> operation-level authorization
  -> open transaction when required
  -> scoped target lookup + mutable-state policy check
  -> verify concurrency precondition and business rules
  -> persist changes + audit + optional outbox
  -> commit, classify outcome, project response
```

Authorization over mutable state occurs inside the write transaction or is protected by an optimistic predicate. Write authority is not inferred from the ability to read a row. The final UPDATE/DELETE statement includes scope and version conditions, not merely the earlier lookup.

Idempotency handling is integrated with this path. Authenticate and apply the operation's replay-disclosure policy before returning any stored response. Claim a key atomically before new side effects, and keep its successful result with the database mutation when feasible.

## 6.3 Transactions and external effects

The transaction handle is explicit and shared by repositories called by the operation. Helpers MUST NOT silently fall back to the pool. Multi-step atomic commands are not assembled through independent generic endpoint calls. Automatic retries are restricted to declared retry-safe transactional work.

External mail, payment calls, and webhook delivery do not belong inside an automatically retried database closure. Write an outbox/job record atomically with the state change and dispatch afterward, or provide an explicitly designed compensation/reconciliation workflow. Commit success is not the same as external delivery success.

## 6.4 Concurrency and idempotency

For representation updates, use a strong ETag/If-Match contract for that target representation, returning 412 on a failed precondition and 428 when the required precondition is absent. HTTP entity tags and conditional request semantics come from RFC 9110; the requirement to supply a precondition is this framework's policy. The 428 status is defined separately in RFC 6585. [R13, R18]

Entity tags must correspond to the selected representation, including relevant representation/security-profile differences; a bare row version is not automatically a valid tag for every serialized variant.

For domain-action POSTs such as publish, use a declared `expected_version` command field unless the action resource has its own carefully specified HTTP validator. Do not casually reuse a GET entity tag as if it were the representation tag of an unrelated action URL. The initial publish operation returns 409 on stale expected_version.

An opted-in Idempotency-Key is scoped by tenant, principal, operation version, and key. Store a hash of canonical input, result/outcome, and retention deadline. Same key with different input is a 409 conflict. Concurrent duplicates are serialized or receive a documented in-progress result. Replaying a stored response cannot bypass current disclosure policy. These are framework-defined semantics, not a claim about universal HTTP behavior.

Distinguish not-started, known-rolled-back, committed, and outcome-unknown. An unknown commit outcome must trigger lookup/reconciliation; the client must not be told to blindly repeat a command.

# 7. HTTP resources, errors, and Axum interoperability

## 7.1 Resource contract

A resource selects a read representation, list/detail query operations, create/update/delete commands, domain actions, policy bindings, filters, ordering, and pagination. Exposing CRUD is explicit. A publish action does not appear merely because a model contains a published Boolean.

```text
GET    /api/v1/lessons
POST   /api/v1/lessons
GET    /api/v1/lessons/{id}
PATCH  /api/v1/lessons/{id}
DELETE /api/v1/lessons/{id}
POST   /api/v1/lessons/{id}/actions/publish
```

List responses use `{items, next_cursor}` initially; a total count is opt-in and scoped. Cursor ordering includes a unique tie-breaker. Cursors are opaque, bounded, integrity-protected, and bound to query shape; authorization is reapplied on every request. Filters, ordering, relation expansions, and maximum page size are allowlisted. No arbitrary SQL, unrestricted field traversal, or client-selected table names.

## 7.2 Error contract

Use application/problem+json with stable framework codes, safe detail text, request ID, and field errors identified by JSON Pointer. Problem Details supplies the representation; the field-error array, retry classification, and operation outcome extensions are framework-defined. [R14]

| Condition | Default response |
|---|---|
| Malformed JSON, wrong primitive shape | 400; no operation executed |
| Unsupported content type or oversized request | 415 or 413 |
| Contract/business input violation | 422 with stable codes and field paths |
| Missing/invalid credentials | 401 with appropriate challenge where applicable |
| Forbidden action or hidden target | 403, or 404 under the resource's concealment policy |
| State conflict, stale command version, idempotency mismatch | 409 |
| Failed/missing required representation precondition | 412 / 428 |
| Rate limit, admission overload, unavailable dependency | 429 or 503 according to documented cause |
| Unexpected internal error | 500; no SQL, secrets, or stack trace |

Failed publication due to workflow state is not necessarily an input-field error. Expose operation-level errors that a generic UI can render without inventing a field. Error codes remain stable across localization.

## 7.3 Custom routes are an intentional escape hatch

A route may bind a normal Axum handler while still declaring authentication, input/output contracts, and operation dispatch. The framework does not need a generic resource abstraction for recommendations, imports, or complex workflows.

Custom routes do not magically gain OpenAPI, field validation, or operation authorization through router mounting alone. A registry check requires an explicit managed binding or a named unmanaged-route waiver. Shared perimeter middleware still applies to mounted unmanaged routes. Production waivers are visible in route inventory and require review; they do not inherit a false security guarantee.

# 8. The interaction metadata protocol

## 8.1 Purpose and separation

The protocol describes how a client may present and invoke registered interactions. It is not a remote copy of all ORM metadata. Four linked pieces are published: a wire contract, an operation/HTTP binding, optional presentation, and contextual availability.

**Wire contract:** JSON Schema for inputs and outputs, codec/validation profile, stable IDs, revision. **Operation binding:** method, path shape, parameter sources, content types, outcomes, concurrency and idempotency requirements. **Presentation:** field order, groups, labels, widget intent, help text, and supported local rules. **Availability:** a caller-specific advisory result and necessary record context, when requested.

Policies themselves, query implementation, private model fields, source paths, secrets, and internal event payloads are never included merely because they exist in the registry.

## 8.2 Versioned envelope

The proposed first stable wire profile is `rustjango.interactions/1.0`; the implementation remains pre-release until conformance tests pass. Protocol version, application contract version, presentation revision, and record version are separate fields with separate lifetimes.

| Envelope field | Meaning |
|---|---|
| protocol / protocolVersion | Reader format and interpretation rules |
| requiredFeatures | Features a renderer must support before allowing submission |
| interactionId / revision | Stable interaction identity and published representation revision |
| audience | Explicit publication profile, such as product or admin; not user authority |
| operation | Operation ID/version, HTTP binding, outcomes, concurrency and retry behavior |
| input / output | Self-contained JSON Schema documents plus stable contract IDs |
| presentation | Optional declarative layout and local widget intent |
| availability / context | Optional actor/record-scoped advisory state and bound values |

JSON objects are returned as application/json. Field names and extension handling are normative in the protocol schema to be implemented. Unknown optional presentation hints may be ignored. Unknown required features, unknown enforcement-related binding semantics, or unsupported required input types must disable the interaction safely rather than dropping fields.

## 8.3 Publication endpoints

```text
GET /_meta/v1/manifest
GET /_meta/v1/interactions/{interaction_id}
GET /_meta/v1/resources/{resource_id}
GET /_meta/v1/resources/{resource_id}/records/{id}/interactions
GET /api/v1/openapi.json
```

A manifest advertises only installed, explicitly published capabilities visible to the caller. Detailed endpoints apply their own policy checks; an ID learned elsewhere does not grant access. Use opaque or strictly parsed IDs and encoded path segments. Public API descriptions and privileged admin metadata are separate publication profiles.

A normal API client can invoke documented endpoints without fetching interaction metadata. The metadata layer is additive, not a mandatory discovery round trip before every API call. Generated forms fetch the needed interaction and submit through its registered HTTP binding.

## 8.4 Static description versus contextual availability

Static describes how an interaction works. Contextual describes whether it currently appears available for a particular actor/record. An absent or forbidden operation need not disclose a detailed reason. Where a disabled operation is visible, return a safe reason code rather than policy internals.

A previous allowed result is never a capability token. Submit-time checks can deny an action that was allowed when the page loaded. Conversely, an API caller is not required to possess a UI enablement flag. The operation's server policy is authoritative in both cases.

Context values can initialize a form or supply an expected record version. They cannot override server-derived identity, tenant membership, price, role, or publication authority. Hidden/disabled controls are still untrusted input.

## 8.5 Caching and metadata safety

The safe initial default for authenticated metadata is `Cache-Control: private, no-store`. Actor/record availability and user-specific initial values must not reach shared caches. Explicitly public, immutable contract assets can be content-addressed and CDN-cached only after an exposure review.

A later private revalidation optimization must key by publication profile, actor/security context, locale, protocol, and relevant policy revision, reauthorize before 304, and clear client caches on sign-out or tenant switch. Vary alone is not the security design.

Bundle schema references locally. The generic browser/server resolver must not fetch arbitrary external `$ref` URLs. Bound document bytes, field count, nesting, array size, rule evaluation, and reference traversal. Reject prototype-polluting property names in dynamic object construction. Metadata labels are text by default; no raw HTML, JavaScript, import URLs, or executable expression strings.

Provisional reference limits are 256 KiB per interaction descriptor, 64 form fields, nesting depth 8, 64 local rule nodes, and 100 items per rendered array. API bodies default to 1 MiB and list pages to at most 100 records. These are configurable starting limits, not measured universal safe values; larger workflows need explicit profiles and load tests. The framework validates limits after decompression where applicable.

# 9. Backend-defined forms and React rendering

## 9.1 The developer experience

A developer registers a form presentation against an existing operation. The form adds labels, ordering, and widget intent; it does not create a second input contract or copy the operation's authorization logic.

```rust
// Proposed presentation builder over generated input field handles.
Form::for_operation::<CreateLessonOp>("lessons.create.form")
    .title("Create lesson")
    .field(CreateLessonInput::title(), Text::new().label("Title"))
    .field(CreateLessonInput::grade_level(),
           Integer::new().label("Grade level"))
    .submit_label("Create lesson")
```

```tsx
// Proposed React package API, not a published package.
<OperationForm
  interaction="lessons.create.form"
  client={api}
  onSuccess={() => lessonsQuery.invalidate()}
/>
```

The React component loads the authorized descriptor, checks protocol and features, selects local renderers, initializes values, runs supported client validation, and posts to the registered binding. Server field errors map back to controls. Success returns a typed or validated result to application code; presentation metadata does not supply executable success callbacks.

## 9.2 Renderer boundary

The browser package contains an allowlisted widget registry. A widget key is a logical request for a locally shipped implementation, not an npm package name or download instruction. Apps may override a renderer by contract/field path without changing backend behavior.

The first set includes text, textarea, checkbox, bounded number, enum select, nullable scalar control, and a basic layout. Relationship pickers come next, through scoped read operations. Unknown optional decoration uses a safe fallback. An unknown essential widget/input capability blocks submission with a useful message and an explicit custom-screen alternative.

Evaluate JSON Forms as the initial rendering engine or adapter; it already accepts separate data and UI schemas. Our operation binding, security posture, compatibility rules, and error transport remain framework-owned. Prove support for the selected schema dialect and keyword subset with fixtures instead of assuming an arbitrary renderer handles all JSON Schema. [R3]

## 9.3 Conditional fields, dirty state, and asynchronous checks

Local rules are a small declarative language over the current input: equality, presence, and bounded Boolean combinations. No JavaScript evaluation, network access, time-dependent logic, or arbitrary code. Visibility is presentation only. A requiredness condition that changes acceptable input belongs in the contract and server validation, not just in a UI rule.

For PATCH, submit only deliberately changed fields and preserve Missing/Null/Value. Do not serialize every displayed default. Hiding a field does not clear stored data unless the interaction defines and validates an explicit clear behavior. In a multi-step form, retained values and step transitions remain local presentation concerns unless the backend defines a durable draft operation.

Remote checks use named, read-only operations with rate limits and scoped errors. Debounce and cancel stale requests; ignore late responses for superseded field values. Re-run authoritative validation on submit. Relationship choices are paginated and permission-scoped, and selected IDs are revalidated during writes.

## 9.4 Accessibility and customization

The renderer must associate labels and errors with controls, provide keyboard navigation and focus management, announce asynchronous status, and support localization, help text, and error summaries. Confirmation dialogs show the actual action target and safe consequences. These are client acceptance tests, not properties of a metadata format.

Custom product screens remain first-class. The framework describes generic interactions, not the whole visual identity or every workflow. A special lesson editor can use the generated SDK and standard operation errors while ignoring the auto-form renderer entirely.

# 10. Administration as an optional client system

## 10.1 Packaging and responsibility

Administration consists of two opt-in components: a backend extension that publishes explicit admin resource/interactions, and a React application that consumes them. The headless core depends on neither. The admin backend depends on core contracts and operations; it does not create an unrestricted CRUD backdoor.

An admin resource declares list columns, visible representations, query operations, allowed filters/sorts, relationship labels, editable operations, and domain actions. A resource can be read-only, expose only a publish action, or omit a generic delete operation. Installing the admin package does not automatically register all models.

The React admin may be deployed as its own CDN bundle or embedded in a product shell. Metadata-driven rendering removes the need for server templates. It does not make navigation, accessible editing, permission-aware queries, concurrency, audit, and dangerous-action handling trivial.

## 10.2 Domain operations, not arbitrary field editing

A Publish button binds to PublishLessonOp. An Invite teacher form binds to InviteTeacherOp. They use the same validation, policy, transaction, audit, and result semantics as calls from a custom React screen or API client.

Separate admin output contracts are allowed. A privileged operator may see audit details that a public API omits. The operation can expose different authorized representations without duplicating its business transition. Never treat possession of the admin bundle or knowledge of an admin URL as privilege.

Bulk operations require explicit all-or-nothing or per-item outcome semantics. Each affected row is scoped and authorized. “All results” requires a server-side selection contract or re-evaluated scoped predicate, not an unbounded client-controlled list. Large exports are queued or bounded and use separately authorized download access.

## 10.3 Audit and operational safety

Successful state changes write audit facts with the domain transaction: actor, effective service identity, tenant, operation/version, target, request ID, and approved redacted changes. Failed or denied attempts are recorded as separate security/operational events. Secret fields and entire request bodies are not copied into audit records by default.

Superuser access is an explicit, separately audited capability. Administrative operations such as restoring data or changing permissions may require step-up authentication and are not generated from model annotations. A database table labeled append-only is not proof of tamper resistance against its administrator.

# 11. Frontend/backend compatibility and generated clients

## 11.1 Standards and schema ownership

Use JSON Schema 2020-12 and OpenAPI 3.1.1 as the initial interoperability baseline, not as a claim that they are the newest revisions. The chosen OpenAPI specification defines a schema dialect based on JSON Schema 2020-12. The exporter must explicitly set its dialect and test the chosen generator/validator toolchain. [R15]

A registry export generates HTTP routes, request and response schemas, documented errors, security schemes, and stable operation IDs. OpenAPI describes authentication mechanisms, not complete record-level authorization. Custom handlers require explicit export bindings. Public documentation excludes private/admin contracts unless deliberately published.

Schemars is an adapter, not the durable schema format. Its documentation permits changes in the exact generated schema shape across releases; therefore pin generator versions, normalize outputs, snapshot exports, and review schema diffs on dependency upgrades. [R8]

## 11.2 Two different frontend modes

**Runtime-driven forms** can display a newly added supported field without rebuilding React. The already deployed renderer fetches a compatible descriptor at runtime. The backend must still deploy the new contract, preserve or version old submissions, and ensure the installed renderer supports the input.

**Generated TypeScript clients** are build artifacts. A new type or operation becomes available after client regeneration and a frontend build. TypeScript checking does not make arbitrary runtime metadata statically typed or validate untrusted network responses. Runtime metadata and responses use explicit decoders where required.

The project may use both modes on the same page. “No manual synchronization” means eliminating duplicate declarations for supported generated interactions, not eliminating versioning, frontend deployments, or custom UX work.

## 11.3 Independent compatibility dimensions

| Dimension | Example | Compatibility rule |
|---|---|---|
| Persistence history | Add nullable database field | Reviewed migration and application schema-capability checks |
| Wire contract | Add required input | Breaking unless old contract behavior is explicitly retained |
| Output contract | Add field or enum variant | Analyze client decoding/exhaustive handling; not automatically safe |
| Interaction binding | Change command target or input mapping | New supported interaction revision and stale-form handling |
| Presentation | Change label/order | No migration; usually backward-compatible within supported widgets |
| Renderer protocol | Require new widget/rule | Advertise required feature; unsupported clients must not submit |
| Record state | Another editor changes lesson | Version/precondition conflict independent of metadata revision |

Stable Rust type names are not wire contract identities. IDs are pinned; renaming a struct does not rename a published operation. Input wire renames require explicit compatibility logic, not merely a model field alias.

## 11.4 Open-form and deployment skew

A generated form submits the interaction revision it used in an `RJ-Interaction-Revision` header. The operation adapter validates that revision against an explicit compatibility map before mutation. Compatible old revisions are accepted under their retained semantics. Unsupported revisions return 409 with code `interaction_stale` and a refresh instruction; no effect occurs.

The form preserves compatible user-entered values, fetches the new descriptor, and asks for review when required fields or behavior changed. It never silently retries a dangerous command under new semantics. Cosmetic label changes do not have to invalidate submissions. The same policy applies when one backend replica is old during a rolling deployment.

Maintain N and N-1 renderer/contract fixtures for every pilot release. Deploy compatible backend expansion first, then new client assets, then remove old behavior after the supported browser/client window. A generic old form can refetch metadata; a compiled SDK still needs its API version supported. Expose release/contract identity for debugging without exposing secrets.

# 12. Security and authority model

## 12.1 Identity is an integration; authorization is core

The reference browser profile uses an existing identity provider integration and server-side session support, not home-grown password/token cryptography. Operations depend on a validated ActorId, tenant membership, and effective capabilities, not one mandatory User table. External API clients can use an explicitly configured bearer/service-identity adapter.

Authentication, collection scope, command permission, writable fields, and output visibility are separate checks. Default denial and checks on every request align with OWASP guidance, but policy correctness remains application work. [R16]

Tenant context comes from verified membership and an authorized selection. A hostname, route parameter, header, or body tenant ID alone is not authority. Create input cannot assign ownership unless the operation explicitly permits it. Foreign-key writes check both visibility and tenant compatibility; composite tenant/key constraints may reinforce this even when model primary keys are single-column.

## 12.2 Browser delivery and session controls

Serving JavaScript from a CDN does not determine the API origin. The preferred deployment exposes app assets, `/api`, and `/_meta` on one public origin through edge routing, with separate upstream services. This permits independent delivery without making CORS an unnecessary architectural requirement.

For separate origins, use a narrow allowlist, explicit credential policy, correct preflight behavior, and trusted proxy configuration. Cookie-authenticated unsafe requests require CSRF defenses, including JSON endpoints. CORS and SameSite alone do not replace that design. Secure/HttpOnly cookies, deliberate SameSite choice, session rotation, expiry/revocation, and origin checks are specified and tested for the selected login flow. [R17]

Metadata never contains bearer tokens, CSRF secrets as static assets, or permission-granting claims. Credential handling lives in the configured client transport. Do not introduce persistent browser token storage merely because React is hosted separately.

## 12.3 Threats and required controls

| Threat | Control and proof |
|---|---|
| Mass assignment / hidden field submission | Explicit inputs, unknown-field rejection, field policy; submit tenant/internal fields adversarially |
| Cross-tenant data or relation inference | Scope SQL before counts, filters, joins and choices; two-tenant fixtures across every path |
| Stale permission or record state | Recheck inside transaction or guarded write; revoke permission while a form is open |
| Metadata leakage or cache contamination | Explicit profiles, auth on descriptors, no-store contextual metadata; cache/tenant-switch tests |
| UI schema XSS or remote-code loading | Text-only labels, local widget allowlist, no scripts/HTML/import URLs; malicious descriptor tests |
| Unsafe metadata URLs / external references | Resolve bindings against configured API origin; allowlist route paths; no external schema fetch |
| Expensive queries or schemas | Query/depth/size/page limits, timeouts, rate/admission control; load and fuzz tests |
| Privileged route bypass | Managed/unmanaged route inventory, reviewed waivers, executor-only generated commands |
| Supply-chain / generated-code mistakes | Lockfiles, dependency review, small diffs, code-generation fixtures, targeted security review |

Read permission does not authorize sorting/filtering on hidden fields: these can disclose values through ordering or membership. Generic listing requires a query-expressible policy or an explicit supported alternative. Unbounded fetch-and-filter-afterward is not an acceptable fallback.

## 12.4 Trust and extension limits

All compiled-in apps and build macros are trusted code. Framework boundaries reduce accidents, not malicious extension behavior. Production builds should reject development impersonation adapters. No custom unsafe Rust, security-sensitive parser, or cryptographic primitive is introduced without a specific reviewed need.

The initial pilot requires external review of policy boundaries, session/CSRF behavior, the metadata resolver, and state-changing operations. Language choice does not substitute for that review.

# 13. Persistence and typed queries

## 13.1 Deliberately narrow ownership

Use SQLx with PostgreSQL for the first real endpoints, initially with explicit repositories and reviewed SQL. Introduce a typed query facade only after two product resources reveal common needs. SeaQuery is a candidate for dynamic, parameterized query construction; it is not a migration-history policy. [R6, R7]

The eventual Model declaration produces desired Schema IR, field handles, and row/insert adapters. It does not automatically produce a public Resource. Define one persistence authority: the model schema is desired state, committed migration operations are historical state, and catalog inspection is observed state.

The first manual migrations use the same manifest/ledger boundary intended for generated migrations. Do not start with one migration system and quietly add a second independent owner of the same tables. The initial code can hand-author state deltas and SQL; automation later emits those same artifacts.

## 13.2 Model and query contract

Stable model and field IDs are pinned before the first committed migration. A Rust rename does not change logical identity. Storage column names, labels, wire names, and stable IDs are separate properties. A label change affects presentation, not the database schema.

Initial persistence supports scalar fields, explicit single-column primary keys, foreign keys, and declared join models. Application defaults, database defaults, and backfill values are distinguished. Unique and referential constraints remain enforced by PostgreSQL, even when application validation gives earlier feedback.

Queries originate from an actor/tenant-scoped repository or context. Filter operands use typed field handles when available; metadata-driven filters validate against an allowlist at runtime. The first query surface is equality/range/null predicates, bounded lists, single-row lookup, selected projections, deterministic ordering, and explicit writes. Advanced traversal and aggregation wait for demonstrated use.

No implicit lazy loading is permitted through property access or serialization. Single-valued eager loading and batched collection loading are explicit. Related-data disclosure policies apply to nested outputs and relationship filters. Visible `.fetch().await` calls can still create N+1 behavior; integration tests assert query counts.

## 13.3 Escape hatches without hidden promises

Privileged SQL remains available in repositories and migration code with explicit review. Raw SQL must use bound values and controlled identifiers. A raw query does not automatically receive generated scoping, output filtering, or migration-state tracking.

The scoped facade restricts ordinary call sites, but trusted application code retains the ability to open a database connection. Document that limit and rely on code review, package boundaries, least-privilege credentials, and integration tests. Defer a custom general-purpose ORM rather than obscuring it inside the first form demo.

# 14. Historical migrations and schema evolution

## 14.1 Preserve the three-state model

```text
D = desired model schema exported from the compiled application
H = historical schema reconstructed from committed migrations
O = observed catalog state of the database

makemigrations : H -> D, producing reviewable artifacts
migrate        : execute committed artifacts in dependency order
schema verify  : compare expected applied state with supported O
```

Django's historical-model mechanism is the conceptual reference: old data migrations should not import current models. This design achieves that through a versioned historical representation and a small independent SDK rather than Python's runtime model reconstruction. [R19]

## 14.2 Artifacts and historical boundary

Each migration has a stable ID, app owner, format version, dependencies, ordered steps, preconditions, logical state deltas, execution mode, reversibility class, and content digest. SQL and frozen data-operation helpers contribute to that digest. Applied history is immutable.

Application migration code lives in a separate crate that depends on the historical SDK and codecs, not the current app. A migration-only binary can replay history without compiling current web services into its runtime. The historical context resolves fields against the schema at the relevant step, not the current schema or the final state of the migration.

The initial historical API supports parameterized selection/update, stable scalar values, explicit inserts/deletes, and bounded iteration. Frozen typed snapshot generation is optional later. Old migrations must remain buildable under a documented supported toolchain/SDK policy; do not promise infinite compatibility with arbitrary future dependencies.

## 14.3 Planner and execution rules

The planner validates IDs and dependency graphs, replays committed state, computes a change set using stable identities, orders supported operations, and classifies risk. Missing required backfill strategy blocks planning. A deleted and newly added field is not assumed to be a rename. Conflicting branch changes require an explicit merge decision.

Use per-object state preconditions rather than one global predecessor hash that would prohibit independent app histories. A release records supported heads and unresolved branches fail deployment checks. Raw SQL requires a logical state delta or an explicit declaration that managed state is unchanged. Unverifiable catalog features are reported as unverified, not a successful match.

The runner acquires a lock on a dedicated pinned PostgreSQL connection, records attempts and step status, and uses transactional DDL where supported. PostgreSQL session advisory locks survive transaction boundaries until release or session termination. Keep that connection pinned for the entire run. [R20]

Nontransactional steps require explicit checks and recovery instructions. Concurrent index creation cannot run inside a transaction block and can leave an invalid index after failure; a general retry or IF NOT EXISTS is not adequate verification. [R21]

Required runner states are pending, running, applied, failed, and needs_reconciliation. Unknown commit outcomes are reconciled against ledger and catalog state. Lock and statement deadlines are explicit. Destructive changes require approval and a tested restore/forward-repair plan. Recreating a dropped column does not recover its lost values.

## 14.4 Rollout and large backfills

Use expand, compatible readers/writers, resumable backfill, validation, and contract/removal. Web startup checks required schema capabilities but never generates/applies migrations. Exact full-schema fingerprint equality is not required when additive schema changes intentionally coexist with old binaries.

Batch checkpoints commit with their writes where possible. A backfill defines behavior under concurrent updates: write pause, dual write, row-version guard plus reconciliation, or another explicit strategy. A high-water mark alone does not settle changes to existing records. External network effects are excluded from ordinary replayable migrations.

**Mandatory proof:** create populated release A; add/backfill a field in B; rename a current model or field in C; remove an obsolete current type; replay all history into an empty database and upgrade A to the same managed schema. The test must run in CI, not only in the initial architecture spike.

# 15. Runtime, jobs, and observability

## 15.1 Execution classes

Distinguish request-scoped asynchronous work, supervised process-local tasks, and durable jobs. A spawned Tokio task is not guaranteed to finish before runtime shutdown. CPU/blocking work needs separate bounded execution and explicit cancellation semantics. [R22]

The initial framework does not need to own a queue implementation. When the product needs durable side effects, integrate a backend that supports transactional enqueue with the existing unit of work, or use an outbox. Do not label a detached task as a reliable notification job.

## 15.2 Durable-job extension contract

The extension records job type/version, ID, payload version, principal/tenant references, schedule, attempts, lease/claim token, expiry, status, and safe error history. Claims are atomic; completion is fenced by claim token; stale workers cannot overwrite a newer attempt's state. Delivery is at-least-once, and external effects require provider idempotency or reconciliation.

Jobs re-evaluate relevant authority at execution or run under an explicitly documented service-authority policy. Never persist a user's bearer token as long-lived permission. Old payload readers remain until old jobs drain or are explicitly migrated. Worker shutdown stops claims and permits a bounded drain; expired work is recoverable.

## 15.3 Observability and failure behavior

Attach request, operation/version, trace, tenant-safe correlation, transaction, and job IDs to structured events. Record latency by stage, error codes, pool waits, query counts, metadata cache behavior, contract mismatch counts, and queue age. Avoid actor/record IDs as unbounded metric labels. Redact request bodies, credentials, and sensitive field values.

Liveness reports process health; readiness reports required dependencies and schema capabilities. A timeout is not proof a database statement or remote operation stopped. Database connections with uncertain state follow driver cleanup rules before reuse. Shutdown and fault tests verify these behaviors.

# 16. Deployment and release topology

## 16.1 Separate delivery, optional shared origin

```text
Browser -> application.example
              |
              +-- /assets/* -> CDN / object storage
              +-- /         -> cached React entry document
              +-- /api/*    -> Rust API upstream
              +-- /_meta/*  -> Rust metadata endpoints

Release job -> migration-only binary -> PostgreSQL
Rust API    -> PostgreSQL
Worker      -> PostgreSQL/outbox -> external services (when needed)
```

The Rust service does not need templates, React build tooling, or application HTML. A standalone convenience distribution may optionally bundle static files outside the core; this is not required for production architecture. The local development command can coordinate the API with Vite using a proxy and separate processes.

## 16.2 Release artifacts

A release records API image/binary, frontend asset digest, contract/interaction export digest, supported protocol/features, migration heads, required schema capabilities, toolchain/dependency lockfiles, and compatibility test results. The process executable is not the whole deployed system: database, configuration, certificates, external identity, and asset storage remain explicit dependencies.

Use separate runtime and migration database credentials. Frontend build assets can be immutable and long-cached; the entry document uses a rollout-aware cache policy. API and personalized metadata are not cached as if they were static assets. The edge must not rewrite unknown API paths to the SPA entry document.

## 16.3 Release gate

Run offline registry checks, contract-diff checks, migration replay/upgrade fixtures, authorization tests, old/new renderer fixtures, and dependency review. Apply reviewed expand migrations, deploy compatible API replicas, publish frontend assets, observe mismatch/error metrics, then perform removal only after the declared compatibility window.

Rollback of application code is permitted only when the active schema and operation semantics support it. Database rollback is a separate decision; forward repair and tested restoration are the default plan for irreversible changes.

# 17. Development experience and inspectability

## 17.1 Repository shape

```text
workspace/
  crates/
    app/                 # Product composition root and CLI
    lessons/             # Models, repositories, contracts, operations
    lessons-migrations/  # Historical code; no lessons dependency
    rj-core/             # App registry and typed execution interfaces
    rj-protocol/         # Portable contract/interaction/schema formats
    rj-historical/       # Historical codecs and migration execution API
  frontend/
    product/             # Normal React application
    interaction-react/   # Optional renderer package
    admin-react/         # Later optional administration shell
  fixtures/              # Protocol, historical and authorization cases
  docs/adr/
```

These are logical boundaries, not a command to publish a dozen crates immediately. Begin with product code plus a small core. Separate historical/protocol code early because accidental coupling there is expensive. Extract modules when tests or reuse justify it.

## 17.2 Proposed commands

```text
cargo run -- check --offline
cargo run -- inspect apps
cargo run -- inspect operation lessons.publish
cargo run -- inspect routes
cargo run -- contracts export --audience product
cargo run -- contracts diff --against release.json
cargo run -- interactions export --audience product
cargo run -- schema export
cargo run -- makemigrations --check
cargo run -- migrate --plan
cargo run -- migrate
cargo run -- schema verify
cargo run -- worker
```

Scaffolding and coordinated development may become a cargo subcommand after the ordinary commands work. Current-schema export requires compilable current declarations; an old export is used only through an explicit artifact input, never silently substituted. History inspection/replay remains available from migration artifacts without starting the product server.

## 17.3 Build a layer only after seeing the code beneath it

For each milestone, first implement an ordinary Rust path, then demonstrate repeated plumbing, then extract the smallest shared abstraction. Favor readable structs, functions, modules, and explicit async calls over elaborate type-state or macro machinery before it has earned its place.

Every abstraction ships with a manual equivalent, an execution trace, and at least one failure-path test. AI-generated code is reviewed in bounded diffs and must compile, pass focused tests, and explain its ownership/error/transaction boundaries. Compilation is necessary evidence, not complete review.

Measure warm save-to-ready latency and build-size growth as resources increase. Keep generated adapters small; use bounded dynamic internals for heterogeneous metadata rather than monomorphizing an entire UI/query engine per model. Source-spanned compile-fail tests are part of framework ergonomics.

# 18. M0–M5 implementation roadmap

Milestones are evidence gates, not calendar estimates. The product can continue using explicit SQL and custom handlers while framework features mature. Authentication is not postponed to the last milestone.

## M0. One real, protected API path

**Build:** Axum/Tokio boot, PostgreSQL repository, reviewed initial migration, configured identity adapter, typed create/retrieve contracts, scoped tenant context, standard errors, and one transaction. Use ordinary Rust and minimal helper functions.

**Show:** an authorized user creates and reads a Lesson; another tenant cannot access it; malformed and unknown fields fail without writes. Demonstrate one rollback and one database constraint failure.

**Do not build:** custom ORM, custom derives, generic viewsets, auto-admin, custom identity provider, queue, or frontend renderer.

**Exit:** the product owner can trace request → policy → repository → transaction → response, and the baseline database setup replays in CI.

## M1. App composition, operations, and contract export

**Build:** explicit App/Project registry, pure assembly, registered CreateLessonOp, portable constraints, Contract IR, JSON Schema/OpenAPI export, and one hand-authored form presentation bound to the operation. Extract the executor from the M0 path without changing behavior.

**Show:** adding a second app validates dependencies; export works with networking disabled; the contract rejects forbidden input identically through manual and managed routes. Missing policy or unknown field references fail checks.

**Do not build:** a general policy language, whole-program macro discovery, production metadata caching, or polished admin.

**Exit:** one operation is invocable and inspectable, and its published contract matches actual decoding/validation fixtures.

## M2. One runtime-generated React form

**Build:** metadata endpoints, versioned envelope, one React OperationForm, local widget registry, standard error mapping, feature checks, operation submission, and stale-interaction handling. Evaluate the existing form-engine adapter using the exact fixtures rather than building a universal renderer.

**Show:** deploy an added optional supported field and its presentation; the existing React bundle displays it after fetching new metadata. Keep an old form open and prove either retained compatible submission or a safe refresh/conflict. Revoke permission between load and submit and verify rejection.

**Do not build:** a low-code designer, arbitrary nested wizard engine, downloaded plugins, visual page layout language, or full admin shell.

**Exit:** the central metadata-driven interaction works end to end with two tenants, no hand-written duplicate validation, and no silent client/backend drift.

## M3. Model metadata and historical evolution

**Build:** stable schema IDs, small Model derive or explicit equivalent, basic field codecs, desired/history diff, historical SDK, migration ledger/recovery, and a small typed query surface. Continue using explicit repositories for unsupported queries.

**Show:** add/backfill/rename replay after current-type removal; concurrent migration runners serialize; a presentation change creates no migration; unsupported destructive changes block generation. If operation automation is not ready, reviewed hand-authored artifacts remain the execution path.

**Do not build:** the entire Django query language, multiple database backends, generic foreign keys, or automatic migration application at server boot.

**Exit:** fresh install and upgrade converge, history is independent, and state ownership is unambiguous.

## M4. Resource ergonomics, SDK, and optional admin

**Build:** narrow DRF-style Resource bindings, reusable list/create/update operations, pagination/filter contracts, generated TypeScript client, scoped relation picker, and an optional admin shell that consumes the existing protocol. Add domain actions rather than turning every internal flag into a writable field.

**Show:** a second product resource is added without hand-wiring repeated HTTP machinery; custom Axum handlers still work; admin and product client invoke the same publish operation. Test patch null/missing, stale edits, forbidden fields, and hidden relation targets.

**Do not build:** a generic CMS, arbitrary bulk workflow designer, runtime native plugins, or metadata-driven replacement for all custom React screens.

**Exit:** measured developer steps decrease, while all previous security and migration fixtures remain green.

## M5. Limited product pilot and production hardening

**Build:** selected production identity/session profile, edge/CDN configuration, rate/admission controls, observability, recovery runbooks, compatibility matrix, and security review. Add transactional jobs/outbox only if the pilot needs durable side effects, then pass their crash/retry tests before use.

**Show:** an old browser bundle, new API replica, additive schema change, and in-flight edit behave as documented. Run load tests with matched application work and a hand-written Axum baseline. Restore from backup and execute a forward-repair drill.

**Do not claim:** general framework maturity, benchmark leadership, or readiness to migrate an established large product merely because the demo works.

**Exit:** explicit pilot approval with known limits, rollback conditions, and owner-reviewed failure behavior.

# 19. Verification, acceptance, and measurement

## 19.1 Required conformance matrix

| Area | Required fixture or failure injection |
|---|---|
| Registry | Stable exports; duplicate IDs; missing dependencies/policies; no network during assembly |
| Contracts | Generated schema versus accepted/rejected JSON; custom codec cases; unknown fields; Unicode and numeric bounds |
| Patch | Missing/null/value; non-nullable rejection; nested/array replacement; untouched defaults not submitted |
| Operations | Same behavior through custom API, generated API and admin; rollback, version conflict, duplicate request |
| Tenant scope | List/detail/count/filter/sort/relation picker/write/bulk paths; hidden-field inference |
| Metadata | Private profile isolation; external refs rejected; unknown required features; size/depth limits; malicious labels |
| React | Error focus, keyboard operation, dirty state, stale async replies, nullable controls, custom renderer fallback |
| Compatibility | Old renderer + new field; stale required input; label-only change; unknown enum/widget; rolling API versions |
| Migrations | Empty replay, upgrade, type removal, branch conflict, drift, changed applied digest, runner crash |
| Runtime | Shutdown, pool exhaustion, request cancellation, uncertain commit outcome, optional worker duplicate effects |

Use real PostgreSQL for schema, lock, transaction, and constraint behavior. Use schema-validation fixtures and property-based tests, not only snapshot equality. Fuzz dynamic filters, metadata parsing, migration manifests, and reference resolution within strict limits.

## 19.2 The three decisive demonstrations

**A. No frontend rebuild:** the same deployed React bundle renders a newly published, supported optional field and successfully submits it. The test records frontend asset digest, old/new metadata revisions, and accepted payloads. This proves a bounded dynamic capability, not unlimited UI forward compatibility.

**B. No alternate authority path:** the same forbidden publish fails through the normal API, generated form, admin, direct operation invocation under an unauthorized context, and worker where applicable. A forged metadata document cannot change that outcome.

**C. No dependency on today's model:** a fresh database replays old data migrations after the corresponding current Rust model type is renamed or removed. The upgrade fixture reaches the same managed schema.

## 19.3 Performance and productivity

There is no accepted claim that this design is faster than FastAPI or Django. Benchmark equivalent SQL, indexes, validation, authentication, authorization, payloads, pool size, hardware, and traffic. Compare against a hand-written Axum path to isolate framework overhead; compare other frameworks only with matched application work.

Record p50/p95/p99 latency, throughput, error rate, memory, pool wait, query count, response bytes, metadata fetch/parse cost, browser render time, warm edit-to-ready time, and clean build growth at 10/100 resources. Report cold and warm conditions separately. Do not interpret a database-bound route as a pure framework benchmark.

Provisional engineering targets, subject to baseline review: no avoidable extra database round trips on generated CRUD; no more than 20% p95 overhead versus the matched Axum baseline on the reference workload; and warm small-fixture save-to-ready p95 under five seconds on the recorded development machine. These are goals, not results or release guarantees.

For productivity, record the code and steps needed to add the second resource, add an input constraint, expose a domain action, and change a supported form field. The target is less duplicated application plumbing with comprehensible code, not the fewest lines at any cost.

# 20. Architect handoff and decision records

## 20.1 Selected direction versus decisions still requiring evidence

**Selected:** API-first core; Django-style apps; separate Model/Contract/Operation/Presentation primitives; optional React clients; one persistence history; explicit policy at execution; incremental product-led implementation.

**Reference defaults:** PostgreSQL + SQLx, Axum/Tower/Tokio, schema normalization through one Contract IR, JSON Schema 2020-12/OpenAPI 3.1.1, same-origin edge routing with separate frontend delivery, and a small form-renderer adapter.

**Requires a spike:** exact schema generator/validator combination, JSON Forms versus a smaller renderer, generic query facade shape, operation adapter ergonomics, migration SDK details, and identity integration for the pilot. Choose versions through compatibility tests, not the word latest.

Cot and Loco remain useful implementation references: their official material describes integrated Rust application frameworks with Django and Rails influence respectively. Review how they solve actual gaps, and adopt components or an extension/fork when it reduces work without erasing the desired learning and ownership. No claim is made that they already satisfy this interaction protocol. [R23, R24]

## 20.2 Required ADRs

| ADR | Decision to record | Evidence |
|---|---|---|
| 001 | Thin owned framework versus adopting/extending a larger framework | Product slice and invariant gap assessment |
| 002 | Contract authority and schema toolchain | Round-trip validation fixtures; exporter/decoder agreement |
| 003 | Operation execution and scoped persistence | Transaction, privilege and tenant test matrix |
| 004 | Interaction protocol, features and compatibility | Old/new renderer and open-form fixtures |
| 005 | Browser identity and deployment origin | Session/CSRF/CORS threat review and integration tests |
| 006 | Historical schema format and migration SDK | Rename/remove/replay and crash recovery |
| 007 | React rendering engine and overrides | Accessibility, supported-schema, error and widget tests |
| 008 | Resource abstraction and raw Axum boundary | Manual versus generated examples; route inventory |
| 009 | Durable execution, when introduced | Atomic enqueue, duplicate delivery, lease and authority tests |
| 010 | Production scope and release approval | Benchmarks, review findings, rollback/restore drill |

## 20.3 Architect return package

Return a refined crate/dependency graph, formal schemas for the three portable representations, operation/adapter interfaces, metadata conformance fixtures, a security threat model, and the exact M0 implementation backlog. Include the measured spikes and rejected alternatives, not only an architecture diagram.

Identify assumptions that the product must supply: tenant selection, identity provider, workflow permissions, data retention, supported clients, deployment platform, and durable side effects. Do not invent business policy from field types. Keep macro spelling and cosmetic UI choices provisional until the manual path and protocol work.

**Approval question:** can an engineer define a controlled operation once, expose it as a useful API, optionally render its interaction in React, evolve its schema safely, and trace every authorization and recovery decision? That is the product we are trying to build.

# Appendix A. One feature, end to end

## A.1 A cohesive feature, not a giant derive

Keep the feature in a `lessons` module with separate `app`, `models`, `repository`, `contracts`, `operations`, `policy`, `api`, and optional `presentation` modules. Tests live alongside the feature. This preserves a cohesive application boundary without hiding all behavior in one derive macro.

The migration crate is adjacent and independent. There is no templates directory requirement. A small product may keep all nonhistorical feature modules in one crate.

## A.2 Resource binding

```rust
// Proposed interfaces: all behavior is registered explicitly.
fn lesson_resource() -> ResourceDefinition {
    Resource::new("lessons.resource")
        .read_with::<ListLessons, GetLesson>()
        .representation::<LessonView>()
        .create_with::<CreateLessonOp>()
        .patch_with::<PatchLessonOp>()
        .delete_with::<DeleteLessonOp>()
        .action::<PublishLessonOp>("publish")
        .policy::<LessonPolicy>()
        .mount("/api/v1/lessons")
        .finish()
}
```

The resource supplies explicit transport bindings and consistent conventions. CRUD helpers may produce ordinary operation implementations for simple records. The publication workflow remains an explicit command, even if its implementation ultimately changes one column.

## A.3 Publish command pseudocode

```text
execute PublishLessonOp(actor, tenant, lesson_id, expected_version):
  authenticate; check operation access; begin transaction
  load lesson through tenant/write scope
  recheck role, publication policy and expected_version
  reject invalid draft-to-published transition
  update with tenant + id + version predicate
  append redacted audit; enqueue notification only if enabled
  commit; return caller-scoped LessonView
```

A generic form may present only a confirmation; an action requiring a reason receives a separate input contract and text control. The product page, admin button, and direct API call share the command. Notification delivery, if enabled, happens after commit and has its own outcome.

## A.4 Change a supported form without a React build

Release B adds optional subtitle with safe omission behavior, a supported text widget, and compatible storage. An existing runtime React form displays it from B's descriptor without rebuilding. An open A form remains valid while A is supported.

A newly required input instead needs a compatible old contract or a new version. Unsupported forms receive a stale-interaction response before mutation. A custom TypeScript screen still needs regenerated types and a rebuild to use new statically named fields. Runtime rendering and compiled clients are intentionally different workflows.

# Appendix B. Proposed interaction specimen

This is a complete JSON example of one create-form interaction, not a formal schema for the entire protocol. The architect must implement the envelope schema, supported-feature registry, and conformance fixtures before declaring protocol 1.0 stable. All URLs below are local example API paths or standard schema identifiers.

```json
{
  "protocol": "rustjango.interactions",
  "protocolVersion": "1.0",
  "requiredFeatures": ["forms.core@1", "http.json@1"],
  "interactionId": "lessons.create.form",
  "revision": "create-form-1",
  "audience": "product",
  "operation": {
    "id": "lessons.create",
    "version": "1.0.0",
    "binding": {
      "method": "POST",
      "path": "/api/v1/lessons",
      "contentType": "application/json",
      "body": "input",
      "interactionRevisionHeader": "RJ-Interaction-Revision"
    },
    "success": {"status": 201, "body": "output"},
    "errors": [400, 401, 403, 409, 413, 415, 422, 429, 500, 503],
    "idempotency": {"mode": "none"},
    "concurrency": {"mode": "not_applicable"}
  },
  "input": {
    "id": "lessons.create.input",
    "version": "1.0.0",
    "validationProfile": "rj.portable@1",
    "schema": {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "type": "object",
      "additionalProperties": false,
      "required": ["title", "grade_level"],
      "properties": {
        "title": {"type": "string", "minLength": 1, "maxLength": 200},
        "grade_level": {"type": "integer", "minimum": 1, "maximum": 12}
      }
    }
  },
  "output": {
    "id": "lessons.view",
    "version": "1.0.0",
    "schema": {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "type": "object",
      "additionalProperties": false,
      "required": ["id", "title", "grade_level", "status", "version"],
      "properties": {
        "id": {"type": "string", "format": "uuid"},
        "title": {"type": "string"},
        "grade_level": {"type": "integer", "minimum": 1, "maximum": 12},
        "status": {"type": "string", "enum": ["draft", "published"]},
        "version": {"type": "string", "pattern": "^[0-9]+$"}
      }
    }
  },
  "presentation": {
    "kind": "form",
    "title": "Create lesson",
    "layout": {
      "kind": "vertical",
      "children": [
        {"kind": "field", "pointer": "/title", "widget": "text", "label": "Title"},
        {"kind": "field", "pointer": "/grade_level", "widget": "integer", "label": "Grade level"}
      ]
    },
    "submit": {"label": "Create lesson"}
  }
}
```

The schema documents are self-contained. Presentation pointers address input instance fields; the registry resolves them against the declared input schema and rejects missing references. Neither input nor metadata contains an assignable school_id. The operation obtains school context from authenticated membership and authorized tenant selection.

No availability field is included in this static example. Retrieving the descriptor requires the publication policy; executing the command always performs operation authorization. A contextual descriptor may add availability and initial values, under the private/no-store policy. For a timed-out create with idempotency mode none, the renderer must not automatically retry.

## B.1 Field-error example

```json
{
  "type": "urn:rustjango:problem:validation",
  "title": "Validation failed",
  "status": 422,
  "code": "validation_failed",
  "request_id": "req-example-001",
  "errors": [
    {
      "pointer": "/title",
      "code": "max_length",
      "message": "Use 200 characters or fewer.",
      "params": {"max": 200}
    }
  ]
}
```

## B.2 Required protocol failure behavior

A descriptor with a missing requiredFeature implementation cannot submit. An unknown optional label hint may be ignored. An input reference that is unresolved, cyclic beyond the supported profile, or external is rejected. A binding that escapes the configured API origin is rejected. A new metadata revision does not authorize an action or supply trusted server identity.

# Appendix C. Sources and verification notes

External references were checked on September 25, 2026. They support statements about existing tools and standards, not the implementation or performance of this proposed framework. Exact dependency versions, schema dialect support, and interoperability must be pinned and reproduced during the initial spikes.

**[P1] Prior project design.** Rustjango Technical Architecture and Design Specification v0.1, September 25, 2026. Used for the preserved migration, identity, query-scoping, execution and historical-independence requirements; the HTML/template direction is superseded here.

**[R1] FastAPI, Features.** Typed declarations, validation, OpenAPI, and client-generation integration. https://fastapi.tiangolo.com/features/

**[R2] Django REST Framework, Metadata.** Existing OPTIONS metadata, field information, configurable metadata classes, and schema endpoints. https://www.django-rest-framework.org/api-guide/metadata/

**[R3] JSON Forms, UI Schema.** Data/UI separation, layout/control metadata, and renderer-specific configuration. https://jsonforms.io/docs/uischema/

**[R4] Rust Reference, Procedural macros.** Local token-based expansion and build-time execution boundary. https://doc.rust-lang.org/reference/procedural-macros.html

**[R5] Axum, crate documentation.** HTTP routing, extractors, and Tokio/Tower integration. https://docs.rs/axum/latest/axum/

**[R6] SQLx, crate documentation.** Async database access and pooling. https://docs.rs/sqlx/latest/sqlx/

**[R7] SeaQuery, crate documentation.** Query construction and parameter binding. https://docs.rs/sea-query/latest/sea_query/

**[R8] Schemars, crate documentation.** Rust-to-JSON-Schema generation, Serde compatibility, dialect configuration, and generated-output stability caveats. https://docs.rs/schemars/latest/schemars/

**[R9] Utoipa, crate documentation.** OpenAPI generation/builders and Axum integration; exact baseline compatibility requires testing. https://docs.rs/utoipa/latest/utoipa/

**[R10] Rust Reference, Traits: dyn compatibility.** Constraints on heterogeneous trait-object registration and dispatch. https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility

**[R11] JSON Schema, Annotations.** Defaults, readOnly, writeOnly, and other descriptive keywords. https://json-schema.org/understanding-json-schema/reference/annotations

**[R12] JSON Schema, String reference.** Length, patterns, and format behavior. https://json-schema.org/understanding-json-schema/reference/string

**[R13] IETF RFC 9110, HTTP Semantics.** Conditional requests, entity tags, If-Match, and response semantics. https://www.rfc-editor.org/rfc/rfc9110.html

**[R14] IETF RFC 9457, Problem Details for HTTP APIs.** Standard error representation and extensions. https://www.rfc-editor.org/rfc/rfc9457.html

**[R15] OpenAPI Specification 3.1.1.** Chosen interoperability baseline and schema dialect; not a claim about the latest published revision. https://spec.openapis.org/oas/v3.1.1.html

**[R16] OWASP, Authorization Cheat Sheet.** Deny-by-default and consistent request authorization. https://cheatsheetseries.owasp.org/cheatsheets/Authorization_Cheat_Sheet.html

**[R17] OWASP, CSRF Prevention Cheat Sheet.** Cookie-authenticated state changes, origin checks and defense considerations. https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html

**[R18] IETF RFC 6585, Additional HTTP Status Codes.** 428 Precondition Required and 429 Too Many Requests. https://www.rfc-editor.org/rfc/rfc6585.html

**[R19] Django 6.0 documentation, Migrations.** Historical models, committed history, dependencies and data migrations. https://docs.djangoproject.com/en/6.0/topics/migrations/

**[R20] PostgreSQL 18 documentation, Explicit Locking.** Session advisory lock lifetime. https://www.postgresql.org/docs/18/explicit-locking.html

**[R21] PostgreSQL 18 documentation, CREATE INDEX.** Concurrent-index transaction restrictions and failure behavior. https://www.postgresql.org/docs/18/sql-createindex.html

**[R22] Tokio, spawn documentation.** Task execution and runtime-shutdown limitations; blocking-task cancellation constraints. https://docs.rs/tokio/latest/tokio/task/fn.spawn.html and https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html

**[R23] Cot, official guide.** Framework positioning and integrated application capabilities; no full conformance evaluation is claimed. https://cot.rs/guide/latest/

**[R24] Loco, official documentation.** Integrated framework reference and Rails-inspired application approach. https://loco.rs/docs/
