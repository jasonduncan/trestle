//! An app on the default state, with no type parameter in sight.
//! `cargo run --example notes -- check`, `-- makemigrations --check`, …

use trestle::prelude::*;

struct Notes;

impl App for Notes {
    fn id(&self) -> &'static str {
        "notes"
    }

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
        .run()
}
