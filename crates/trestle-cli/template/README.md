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

After editing models in `trestle.toml`: `trestle makemigrations`, then
`trestle check`. `cargo test` boots the server against a throwaway database.
