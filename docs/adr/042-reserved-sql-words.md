# ADR-042: Reserved SQL words are refused as names, not quoted

**Status:** Proposed (2026-10-22) · **Design doc:** §3.4, §13.2, §14 ·
**Amends:** the intent format, the schema builders · **Depends on:**
ADR-024, ADR-030

## Context

Every table, column, constraint and index name is checked to be a plain
lowercase identifier and then emitted unquoted: `trestle_historical::sql::ident`
in migrations (`crates/trestle-historical/src/sql.rs`, lines 12–18 at
0.7.1), and the intent's runtime SQL in `crates/trestle/src/intent/runtime.rs`.
A plain identifier can still be a PostgreSQL key word. A field
`{ id = "primary", type = "bool" }`, `{ id = "order", type = "int" }` or a
model with `table = "user"` passed `trestle check` and produced SQL that
PostgreSQL refuses: `CREATE TABLE deals (..., order bigint ...)` is a syntax
error. The author found out at `migrate`, or, for names that only break
in queries, at the first request, as a 500.

Which words break, measured against PostgreSQL 16 by running every key
word of `pg_get_keywords()` and every `pg_catalog` type name through the
shapes Trestle emits (`CREATE TABLE`, `ALTER TABLE ... ADD/RENAME/DROP
COLUMN`, `CREATE INDEX`, `CHECK`, and the intent's `INSERT ... SELECT ...
FROM jsonb_populate_record(NULL::<table>, ..)`, `UPDATE ... SET (..) =
(SELECT ..)`, `SELECT ... FROM <table> t WHERE t.<column> ...`, `FOR
UPDATE OF t`):

- **Reserved** and **reserved (can be function or type)** key words (78
  and 23 in PostgreSQL 16) fail everywhere, as columns and as tables.
- **Non-reserved (cannot be function or type)** key words (`position`,
  `time`, `values`, `between`, ...) work as columns in every shape. As a
  table they fail in `NULL::<table>`, which parses them as type syntax.
- **`pg_catalog` type names** (`text`, `date`, `money`, `uuid`, `point`,
  ...) work as columns, and as tables in DDL, but `NULL::<table>`
  resolves to the built-in type, since `pg_catalog` is searched first.
- Other non-reserved key words work everywhere.

## Decision

- **One explicit list per class, in one place:**
  `trestle_historical::schema::RESERVED_WORDS` (reserved and "can be
  function or type", from the PostgreSQL documentation's "SQL Key Words"
  appendix, Appendix C, Table C.1, for PostgreSQL 16, which reserves
  everything 13 does plus `system_user`) and `TYPE_NAMES` (the
  "cannot be function or type" key words, with the JSON and
  `merge_action` ones PostgreSQL 17 adds, and the `pg_catalog` type
  names). Both are sorted constants with `reserved_word` and `type_name`
  lookups, and a comment saying where they come from and when to refresh
  them. CI runs PostgreSQL 13 and 17.
- **The intent file refuses them when it is read,** with the line and a
  suggestion:
  - a model field that is a reserved word: `field "primary" is a reserved
    SQL word; choose another ID, e.g. "is_primary"`. The suggestion
    follows the type: `is_<id>` for a bool, `<id>_id` for a ref or uuid,
    `<id>_at` for an instant, `<id>_on` for a date, `<model>_<id>`
    otherwise;
  - a model's table, given or derived (`<name>s`), that is a reserved
    word or a type name: `table "group" is a reserved SQL word; choose
    another table, e.g. table = "groups"`;
  - a tenancy model whose `<name>s` table is either.

  Handler input fields never reach SQL, so they aren't checked.
  Non-reserved key words such as `status`, `position` or `time` stay
  valid field IDs.
- **The Rust builders share the reserved list.** `Model::problems`, which
  checks every registered model at assembly, refuses a reserved word as a
  table, a field key, a storage column or a constraint or index name.
  Such a model could never have been migrated, so no working project
  changes. It doesn't refuse type names: Rust apps don't use the intent's
  `NULL::<table>` casts.
- **`sql::ident` refuses a reserved word too,** so rendering a migration
  never emits one even if a check upstream is missed.
- **Identifiers stay unquoted.** Quoting every identifier
  (`"order"`) would make any name legal, but it would change the SQL text
  of every migration `makemigrations` renders, and the classifier compares
  a migration's SQL with what Trestle renders for its operations, so every
  existing project's committed migrations would read as hand-edited. Hand
  SQL, data steps and the runtime would all need quoting rules too, and a
  quoted name is a trap for anyone querying the database by hand.
  Refusing the few words that break is cheaper and keeps the SQL readable.
  If quoting is ever adopted, it needs its own ADR and a migration-format
  change.

## Evidence

- `crates/trestle-historical/src/schema.rs`,
  `reserved_words_are_refused_as_names`: both lists are sorted, unique
  and plain identifiers; `order` and `user` are reserved and `status`
  isn't; a Rust model with table `user`, a field `primary`, a storage
  column `order` and an index named `select` reports exactly those four
  problems, and a plain model none.
- `crates/trestle/tests/intent.rs`,
  `reserved_sql_words_are_refused_with_their_line`: `primary` (bool),
  `order` (int), `user` (uuid), `end` (instant), `table = "group"` and
  `table = "text"` give exactly six problems, each with its line and
  suggestion; `status` and `position` are accepted, and the file with the
  suggested names parses.
- No fixture, example, test, the pilot or its committed migrations use a
  refused name (a scan of field IDs, tables, `Column::` keys and migration
  JSON), and the full suite passes unchanged.

## Consequences

- **A name that would break PostgreSQL is a `check` problem,** with a
  line and a fix, instead of a failed `migrate` or a 500.
- **The lists are PostgreSQL's,** so they need a look when CI's newest
  PostgreSQL changes. A word that becomes reserved later would make an
  existing project's file fail to parse; PostgreSQL rarely reserves new
  words for that reason, and the change would be a breaking-change note.
  The lists already include words only newer versions reserve
  (`system_user` from PostgreSQL 16, and the JSON and `merge_action` words
  from 17), so a project on an older PostgreSQL is refused a few names that
  would work there today but would break on an upgrade.
- **Breaking:** an intent file whose table is a `pg_catalog` type name or a
  "cannot be function or type" key word (`table = "date"`) used to pass
  `check`, migrate, and then fail on its first write. It is now refused
  when read.
