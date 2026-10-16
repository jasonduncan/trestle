# Changelog

All notable changes to Trestle are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
change the API. The migration file format, the CLI's JSON output and its
exit codes are versioned separately (`trestle.plan/1`, `trestle.verify/1`,
`trestle.migrate/1`, manifest `format`) and are kept compatible.

Projects depend on the single `trestle-framework` package, imported as
`trestle`. The other crates (`trestle-core`, `-historical`, `-identity`,
`-protocol`, `-mail`) are internal, pinned exactly, and carry no stability
promise.

## [Unreleased]

## [0.2.0] - 2026-10-15

### Added
- Licensed under MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`,
  `NOTICE`); `cargo deny` checks dependency licenses.
- `SECURITY.md` and `CONTRIBUTING.md`; a README for newcomers. The pilot
  example's material moved to `examples/pilot/README.md`.
- CI runs the frontend tests and typecheck.

### Changed
- Prepared for crates.io (not yet published): the facade package is
  `trestle-framework` with `[lib] name = "trestle"`, so imports, the binary,
  `trestle.toml` and `TRESTLE_*` are unchanged. Internal crates use
  `version = "=0.2.0"` path dependencies; every publishable crate has a
  README, keywords, categories and an `include` list; docs.rs builds with all
  features. The workspace version is 0.2.0.
- `trestle new` depends on the crates.io release by default
  (`trestle = { package = "trestle-framework", version = "0.2" }`);
  `--git`/`--tag`/`--trestle-path` override it. New projects declare
  `rust-version` instead of pinning a toolchain.

### Fixed
- The scaffold template README shows the right `trestle tenant grant` flags.
- The migrations integration test works against a PostgreSQL that needs a
  password.

## [0.1.0] - 2026-10-15

The first release.

### Added
- **Foreign apps**: `Project::foreign_apps` / `MigrationSet::with_foreign_apps`
  leave ledger rows of apps that live in another project alone (`migrate`
  no longer refuses them as "unknown applied"); `verify --json` lists them
  under `foreign`.
- **`trestle` facade crate** (F1):
  - `trestle::project().run()` is the project binary's command line
    (`serve`, `check`, `migrate`, `makemigrations`, `openapi`, ...), in the
    style of Django's `manage.py`.
  - `trestle::prelude`, a default `State`, and `App` without a type
    parameter.
- **Runtime migration loading** (F2):
  - `migrations/<app>/<id>.{json,sql}` are read at runtime, or embedded with
    `trestle::embed_migrations!`.
  - Data steps are registered by name with `app.data_step(...)`.
  - Digests are compatible with existing ledgers.
- **Intent file `trestle.toml`** (F3):
  - It declares models, CRUD endpoints, roles and workspace tenancy, and is
    read at runtime.
  - Rust handlers and hooks are linked by name, with startup errors that
    carry a line number.
  - New pieces: a row accessor, `If-Match` concurrency, and the
    `tenant create/grant/revoke` commands.
- **`trestle` wrapper and project template** (F4):
  - The `trestle-cli` crate builds the `trestle` command, and
    `trestle new NAME` scaffolds an intent-based project.
  - Password sign-in is on by default. Google/OIDC is opt-in through
    `[auth] oidc`, which names environment variables, never secrets.
  - `trestle::testing::TestApp` (feature `testing`) supports a project's own
    tests.
- **Non-atomic migrations** (F5):
  - `"atomic": false` runs one statement per step, separated by
    `-- trestle:step` lines.
  - The ledger records `step` and a `partial` status, and a rerun resumes
    where it stopped.
  - An invalid index left by a failed `CREATE INDEX CONCURRENTLY` is dropped
    before the retry.
  - Manifests can set `lock_timeout` and `statement_timeout`.
- **Reconciler contract** (F6):
  - `migrate --plan --json` prints `trestle.plan/1` with an auto/approval
    classifier.
  - `migrate --approve <digest>`, `migrate --auto`, `--backup-ref` and
    `--lock-timeout`.
  - `verify --json` prints `trestle.verify/1`.
  - The exit codes are stable (0–7, 10).
- **CI** (F7):
  - fmt and clippy.
  - Tests on PostgreSQL 13 and 17.
  - Pilot `makemigrations --check`, replay from empty, and upgrade from the
    last tag.
  - A `trestle new` scaffold job.
  - `cargo deny`.
  - A release consistency check on tags.

### Changed
- The pilot product is now an example: `examples/pilot`,
  `examples/pilot-migrations`, `examples/loadtest`. Its Smart Folder backend
  moved to its own repository.
- In production, plain `migrate` applies only plans the classifier marks
  `auto`. Anything else needs `--approve <digest>`.
- `serve` refuses to start on schema drift or a partly applied migration.

[Unreleased]: https://github.com/jasonduncan/trestle/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jasonduncan/trestle/releases/tag/v0.1.0
