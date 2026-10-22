# ADR-034: Intent projects install the identity app

**Status:** Proposed (2026-10-21; design agreed by the project owner) ·
**Amends:** the intent API, ADR-005 · **Depends on:** ADR-014, ADR-030

## Context

In 0.7.1, a `trestle.toml` project could not sign anyone in:

- `[auth] passwords` defaults to on. `serve` turns password sign-in on
  from it (`ServeConfig::apply_auth`) and so requires
  `TRESTLE_SESSION_KEY` and `TRESTLE_PUBLIC_ORIGIN`
  (`crates/trestle/src/serve.rs`).
- Nothing installed `trestle_identity::IdentityApp`, which owns the
  account, link, login and session tables and the `/api/v1/auth/*` routes.
  Only a Rust project that installed it by hand could sign in
  (`examples/pilot/src/lib.rs`).

So the sign-in routes answered 404, and `account invite` failed with
`relation "identity_accounts" does not exist`. Proseware, which builds
`trestle.toml` projects, needs password sign-in to work out of the box.

## Decision

- **`[auth]` decides.** When `[auth]` turns on browser sign-in,
  assembly installs `IdentityApp` after the intent's apps. Browser
  sign-in means `passwords` (the default), `oidc`, or both
  (`AuthSection::sign_in`). Both modes need the app:
  - password sign-in uses the account, link and session tables and the
    `/api/v1/auth/password/*` routes;
  - OIDC sign-in uses the login-state and session tables and the login
    and callback routes.
  `session`, `methods` and `logout` serve both.
- **Off means absent.** With `passwords = false` and no `oidc`, nothing is
  installed: no identity tables and no `/api/v1/auth/*` routes. Such a
  project authenticates with development tokens or
  `TRESTLE_OIDC_API_AUDIENCE` bearer tokens, which need no tables.
- **Migrations work like any app's.** The identity models are planned
  with the intent's models. `makemigrations` writes `identity/0001_*` into
  the project's migrations directory, to be reviewed and committed, and
  `migrate` applies it. The installed app declares no migration
  references (`IdentityApp::new(&[])`). As with intent apps, `serve`
  requires every committed `identity` migration to be applied.
- **No second copy.** If the project installs an app with the ID
  `identity` itself, assembly keeps that one, with its own migration
  references, and adds nothing. While sign-in is on, intent models may
  not use the app ID `identity`; `check` reports that as a problem.
- **`serve` refuses a sign-in it cannot back.** If password sign-in or an
  OIDC client is configured and the registry lacks the identity app (the
  app `identity` with each of `trestle_identity`'s models as it declares
  them; through 0.8.1, any model named `identity.session` passed), `serve`
  refuses to start. This covers a Rust project without
  `IdentityApp`, and `TRESTLE_OIDC_CLIENT_ID` set for a project whose
  `[auth]` is off. Before, such a server started and failed at the first
  sign-in.
- **State.** `Project::assemble`, `run` and `run_matches` now require
  `Browser: FromRef<S>`. `State` already provides it, and `StateParts`
  carries the `Browser` that a custom state needs.
- **Sign-in code is unchanged.** These are the same routes and the same
  code as the pilot's. Passwords go only through
  `trestle_identity::password` (Argon2id). Every sign-in failure is
  identical. Cookie writes need `Origin` and `X-CSRF-Token`. Link secrets
  are stored hashed. The production boot checks are unchanged. Identity
  yields only an actor: generated endpoints still resolve the tenant from
  the caller's memberships and check the role at execution.

The admin shell (`@trestle-framework/admin-react`'s `AdminHost`) already
reads `/api/v1/auth/session` and `/api/v1/auth/methods` and shows the
password form, so the server needs nothing more. A deployment still has to
provide a few things:

- The shell must be served from an origin listed in
  `TRESTLE_PUBLIC_ORIGIN`, on the same origin as the API, for example
  through the Vite proxy or the edge.
- The person needs a membership (`tenant grant`). The shell also needs a
  tenant discovery read for its `tenantProfile`, such as the directory app
  that `trestle new --admin` writes.
- Invite and reset links open `/set-password` on the first public origin.
  `AdminHost` does not render that page, so the origin must serve
  `SetPassword` from `@trestle-framework/interaction-react`, as the
  product app does.

## Consequences

- A project from `trestle new` signs in with a password out of the box:
  `trestle account invite`, set the password from the link, then sign in.
  The template and `examples/notes_intent` now commit
  `identity/0001_initial`.
- An existing intent project with the default `[auth]` must run
  `makemigrations` once after upgrading, and review and commit the
  identity migration; until then, `check` reports that models differ from
  committed migrations. A project that wants no sign-in sets
  `passwords = false`.
- A project on its own state type must implement `FromRef<S>` for
  `Browser`.
- Tests:
  - `crates/trestle/tests/intent_sign_in.rs` takes a default-`[auth]`
    intent through the commands `makemigrations`, `migrate`,
    `account invite` and `tenant grant`. It then sets a password and signs
    in over HTTP, and the session creates and lists records through
    generated endpoints. The same session is refused (403) without
    `X-CSRF-Token` and in a workspace where it has no role.
  - The same file checks, offline: OIDC alone installs the app; sign-in
    off installs no tables or routes, and `serve` refuses password
    sign-in set by the environment; a hand-installed `IdentityApp` stays
    the only one; and the taken app ID is reported.
  - `examples/pilot/tests/intent_parity.rs` now also checks the identity
    tables against pilot's migrated database.
