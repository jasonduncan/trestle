# ADR-018: Declared effects, and reads that cannot write

**Status:** Proposed (2026-10-17) · **Design doc:** §6.1, §6.2 · **Amends:**
ADR-003 · **Depends on:** ADR-014, ADR-015

## Context

An operation's `Mode` said whether it was a read or a write. Nothing else
recorded what it touched:

- **No answer to "what does this change?"** No tool could say which
  operations write `lessons.lesson`, which jobs an operation enqueues, or
  which external systems it calls. Those are the questions a reviewer, an
  auditor or an upgrade plan asks.
- **`Mode::Read` was only a label.** A read ran on a pooled connection in
  autocommit, so a bug that wrote from a read committed silently.

## Decision

- **Declaring effects is optional.**
  - A typed operation declares
    `const EFFECTS: Option<EffectsInfo>`, made of `&'static` slices:
    `EffectsInfo::new().reads(&[..]).writes(&[..]).emits(&[..]).egress(&[..])`.
    It sits next to the code that has those effects and defaults to `None`.
  - A runtime-built operation declares them on its descriptor:
    `OperationDescriptor::effects(Effects)`. This follows ADR-014's
    owned-data rule.
  - The registry keeps an owned `Effects` (sorted, unique) on every
    registered operation.
  - Effects describe `run`. Context resolution and policy lookups are not
    part of them.
- **What each list means:**
  - `reads` and `writes` are model IDs. Writing a model implies it may also
    be read.
  - `emits` lists jobs the operation enqueues: operation IDs or job handler
    kinds.
  - `egress` names external systems, such as `"smtp"` or a host.
- **Checked at assembly:**
  - every named model is installed;
  - every `emits` entry is a registered operation or job kind;
  - no `egress` entry is empty;
  - a read declares no writes or emits.
- **Checked at run time:** a write that declared its effects may enqueue
  only what its `emits` lists. The executor puts the declared list in a task
  local around `run`. A typed operation's list is its `const`, so nothing is
  allocated. `jobs::enqueue` refuses anything else with a 500, and the write
  rolls back. An operation that declares nothing is unchecked, and so is an
  enqueue made outside an operation.
- **Not enforced:** `writes` within a write, and `egress`, are recorded but
  not checked. Trestle sees neither the tables a statement touches nor the
  network.
- **Reads that cannot write.** With `db::read_only_reads()` on, every read
  runs in `BEGIN READ ONLY`, declared or not.
  - **A write from a read fails:** it gets PostgreSQL `25006`, logged as "a
    read operation tried to write", and the caller sees a 500.
  - **The `ROLLBACK` costs no round trip:** the transaction is dropped
    rather than committed. sqlx queues its `ROLLBACK`, and the pool sends it
    with the check it already makes when a connection comes back.
  - **The extra `BEGIN` is still a full round trip.** On the load
    comparison that cost about a fifth of read throughput:
    - get, at concurrency 8, fell from 12,534 to 9,611 req/s;
    - list fell from 11,151 to 8,834 req/s;
    - p95 overhead against the matched baseline rose from about 6% to
      23–31%, past the §19.3 target.
  - **So the default depends on the build.** It is on in debug builds,
    which means tests and development, where a misdeclared read is caught.
    It is off in release builds.
  - **Production can opt in** with `trestle serve --read-only-reads`
    (`TRESTLE_READ_ONLY_READS`) or `db::set_read_only_reads(true)`, and
    accept that cost.
  - **A cheaper way would be a dedicated read pool** whose sessions default
    to read only. It costs nothing per request, but it splits the connection
    budget. It can also point at a replica. It is left for a later change.
  - **`DynConn`** keeps `Read` (a pooled connection) and gains `ReadOnly`
    (the read-only transaction).
- **Shown by:**
  - `inspect operation <id>`, under `"effects"`;
  - `inspect effects [--model <id>]`, a table of every operation's
    declaration, or only the operations that touch one model.
- **`trestle.toml`:**
  - Generated list and get declare `reads` of their model.
  - Generated create, update and delete declare `writes` of their model
    unless a hook runs on that operation. Hooks are Rust code, so such an
    operation declares nothing.
  - Handler endpoints declare nothing.
- **The pilot** declares effects on every operation, and a test keeps it
  that way.

## Evidence

`crates/trestle-core/tests/effects.rs` (PostgreSQL):

- with read-only reads on, a typed read and a dynamic read that insert are
  refused, nothing is written, and the pool's connections stay usable.
  With it off, they run on a plain connection, as before;
- a declared write may enqueue its declared job. Another job is a 500 and
  rolls the write back. An undeclared write and an enqueue outside any
  operation are unchecked. A dynamic write is checked the same way;
- `inspect` shows typed and dynamic declarations, and `null` for none;
- assembly refuses a read that declares writes, an unknown model, an
  unknown emitted job and an empty egress;
- declarations are sorted and unique.

`crates/trestle/tests/intent.rs` checks the generated declarations and the
hook exception. `examples/pilot/tests/registry.rs` checks that every pilot
operation declares its effects, and that the operations writing
`lessons.lesson` are exactly create, update, delete and publish.

## Consequences

- **Breaking (behavior):** in debug builds, and wherever production opts
  in, a `Mode::Read` operation that writes fails. Such an operation was
  already misdeclared. Make it a `Mode::Write`. A release build that does
  not opt in behaves as before.
- **Breaking (API):** `DynConn` has a new `ReadOnly` variant, so an
  exhaustive `match` needs an arm. Code that only calls `conn()` is
  unaffected.
- **Debug and release differ, on purpose,** the way `debug_assert!` does.
  Test suites run debug builds, so they catch a read that writes before it
  ships.
- **Trust in declarations:** `emits` is only as strong as its scope. A
  write that spawns its own task escapes the check, but spawning from a
  write is already forbidden (ADR-015).
- **What a declaration is for:** inspection and review. It grants nothing
  and does not replace authorization.
