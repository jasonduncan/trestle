# {{name}}

A [Trestle](https://github.com/jasonduncan/trestle) API server. `trestle.toml`
declares it; `src/main.rs` registers any Rust handlers.

`trestle new` wrote a `.env` with fresh development keys and one development
token: `TRESTLE_DEV_TOKENS=<token>=<actor-id>`. The part after `=` is your
actor ID; the part before it is the bearer token for requests.

```sh
createdb {{name}}            # DATABASE_URL in .env points here
trestle migrate
trestle tenant create --name "My workspace"       # prints the workspace ID
trestle tenant grant --tenant <workspace-id> --actor <actor-id> --role member
trestle serve
```

Then call the API with the development token and the workspace:

```sh
curl -H "Authorization: Bearer <token>" \
     -H "Trestle-Tenant: <workspace-id>" \
     http://localhost:8000/api/v1/notes
```

People sign in with a password (`[auth] passwords`, on by default).
Invite someone, then grant their actor ID a role as above. The printed link
opens `/set-password` on `TRESTLE_PUBLIC_ORIGIN`, so that origin needs a
frontend page for it, such as `SetPassword` from
`@trestle-framework/interaction-react`:

```sh
trestle account invite --email you@example.com     # prints a one-time link
trestle account list                               # shows the actor ID to grant
```

After editing models in `trestle.toml`: `trestle makemigrations`, then
`trestle check`. `cargo test` boots the server against a throwaway database.
