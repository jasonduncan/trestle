# Trestle

An API-first Rust application framework with the integrated feel of Django
and Django REST Framework, serving JSON only: apps, typed contracts,
authorized operations, tenant scoping in SQL, and one replayable migration
history on PostgreSQL.

The package is `trestle-framework`; the library is imported as `trestle`:

```toml
[dependencies]
trestle = { package = "trestle-framework", version = "0.3" }
```

```rust,no_run
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
    trestle::project().install(Notes).named("Notes", "0.1.0").run()
}
```

Start a project with the CLI: `cargo install trestle-cli`, then
`trestle new NAME`. Enable the `testing` feature in dev-dependencies for
`trestle::testing::TestApp`.

Pre-1.0: minor versions may change the API. MSRV 1.98. See the
[repository](https://github.com/jasonduncan/trestle) for guides, the
changelog and the security policy.

Licensed under either of Apache License, Version 2.0 or MIT license, at your
option.
