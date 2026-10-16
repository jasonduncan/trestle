# Trestle

Trestle is an API-first Rust application framework. It aims for the integrated
feel of Django and Django REST Framework, but serves JSON only: no templates
and no server-rendered forms. You declare apps, typed contracts, authorized
operations and models, and you get one migration history. Clients such as
React forms, an admin or custom screens are separate and optional.

**Status:** 0.x and pre-1.0, so the API can change between minor versions (see
[`CHANGELOG.md`](CHANGELOG.md)). CI runs against PostgreSQL 13 and 17.
The MSRV is Rust 1.98: the code uses edition 2024, let-chains and
`AsyncFnOnce`. Trestle has not had an independent security review yet.

## Start a project

```sh
cargo install trestle-cli   # until the crates.io release: --git https://github.com/jasonduncan/trestle --tag v0.2.0 trestle-cli
trestle new notes && cd notes
createdb notes
trestle migrate
trestle serve
```

`trestle new` writes `trestle.toml` (models, CRUD endpoints, roles,
tenancy), a small `src/main.rs`, committed migrations, and a `.env` with
fresh development keys and a development token. The generated `README.md`
walks through the first tenant and the first request.

To depend on Trestle directly (the package is `trestle-framework`; the library is `trestle`):

```toml
[dependencies]
trestle = { package = "trestle-framework", version = "0.2" }
# or from git:
# trestle = { package = "trestle-framework", git = "https://github.com/jasonduncan/trestle", tag = "v0.2.0" }
```

## A small example

```rust
use trestle::prelude::*;

struct Notes;

impl App for Notes {
    fn id(&self) -> &'static str { "notes" }

    fn register(&self, app: &mut AppBuilder) {
        app.model(
            Model::new("notes.note")
                .table("notes")
                .field(Field::uuid("id").primary_key())
                .field(Field::text("title").chars(1, 200)),
        );
    }
}

fn main() -> std::process::ExitCode {
    trestle::project()
        .install(Notes)
        .named("Notes", env!("CARGO_PKG_VERSION"))
        .run() // serve, check, migrate, makemigrations, openapi, ...
}
```

The same project can be declared in `trestle.toml` instead, with Rust
handlers and hooks linked by name. See `crates/trestle/examples/` for both
styles.

## Core concepts

- **Apps** register models, operations, routes and migrations. A project
  assembles them, and assembly is pure and checked: missing dependencies,
  policies or fields are startup errors.
- **Contracts** (`ContractDoc`) define a wire format once. The same
  definition validates requests, rejects unknown fields, and exports JSON
  Schema, OpenAPI 3.1 and form descriptors.
- **Operations** name their input, output, context and **policy**. Every
  caller goes through one executor: validate, admit, open a transaction,
  recheck, run, commit. An unknown commit outcome is reported as such.
- **Tenancy** is scoped in SQL. A tenant header only selects among the
  caller's memberships, and other tenants' records are 404.
- **Migrations** are planned from the declared models, written as
  reviewable artifacts, and replayed from history. Applied migrations are
  digest-checked, and non-atomic steps resume safely.
- **Identity** yields only an actor. Trestle provides server-side browser
  sessions with CSRF checks, password accounts (Argon2id), OIDC sign-in,
  JWT bearer tokens and scoped API keys.

## Known limitations

- **Credentials an application issues itself.** `trestle_identity::password::sign_in`
  checks a password for clients that hold their own credential, such as a
  native app's token. A credential the application issues after that lives
  outside Trestle, so it can briefly outlast a later disable. The issuing
  application must handle that, for example by rechecking the account or by
  keeping those credentials short-lived. Disabling an account does revoke
  its Trestle sessions, links and API keys.
- Rate limits are per process and in memory. Use edge limits as well.
- See [`SECURITY.md`](SECURITY.md) for how to report a vulnerability.

## Default limits

These built-in defaults are a starting point and safe to adjust for your
deployment (see `docs/runbooks/operate.md`):

| Limit | Default | Setting |
|---|---|---|
| Unauthenticated requests per client address | 20/s, burst 60 | `TRESTLE_ADDRESS_RATE_PER_SECOND`, `TRESTLE_ADDRESS_BURST` |
| Signed-in requests per actor (API key or session) | 20/s, burst 40 | `TRESTLE_RATE_PER_SECOND`, `TRESTLE_RATE_BURST` |
| Password sign-in attempts per client address | 30/min | `TRESTLE_PASSWORD_ATTEMPTS_PER_MINUTE` |
| Password sign-in attempts per account | 10/min | `PasswordPolicy::account_attempts_per_minute` |
| Lockout | 5 consecutive failures, 15 min | `PasswordPolicy` |

## Repository layout

```text
crates/
  trestle/             the facade crate projects depend on (project CLI, intent file, testing)
  trestle-cli/         the `trestle` command: `trestle new`, then runs the project's own CLI
  trestle-core/        registry, executor, managed routes, exports, problem+json, admission
  trestle-protocol/    portable formats: Contract IR, JSON Schema, forms, interaction descriptors
  trestle-historical/  schema IR, planner, SQL, migration artifacts, ledger/runner, verify
  trestle-identity/    sessions + CSRF, password accounts, OIDC, JWT bearer, API keys
  trestle-mail/        outgoing email: SMTP (TLS) and file backends
examples/
  pilot/               a full example product (schools, lessons, admin); see its README
  pilot-migrations/    the pilot's committed migration history
  loadtest/            load comparison against a hand-written Axum baseline
frontend/              React renderer, product app and admin shell for the pilot
docs/                  guides, ADRs, runbooks, design history
deploy/                example edge config, systemd unit, production environment template
```

The pilot example and its demonstrations are described in
[`examples/pilot/README.md`](examples/pilot/README.md).

## Development

You need Rust (see `rust-toolchain.toml`) and a running PostgreSQL 13 or
newer that your user can create databases in. Integration tests create and
drop `trestle_test_*` databases. Set `TRESTLE_TEST_DATABASE_URL` if the
server is not at `postgres://localhost/postgres`.

```sh
make test        # cargo test --workspace
make lint        # rustfmt + clippy -D warnings
make frontend frontend-test
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the rules every change keeps.

## Design history

Trestle was called Rustjango during design. The original design document is
kept as history in [`docs/design/`](docs/design/). Where it differs from
the code, the ADRs in [`docs/adr/`](docs/adr/) record what was actually
built.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option. Third-party notices are in
[`NOTICE`](NOTICE).

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in Trestle by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
