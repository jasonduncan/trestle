//! The CRUD server from design §3.5: a `trestle.toml` and a few lines of
//! Rust. `cargo run --example notes_intent -- check`.

use trestle::prelude::*;

/// `POST /api/v1/notes/{id}/pin`
async fn pin(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.update("notes.note", input.id()?, json!({ "pinned": true }))
        .await
}

/// Runs before every create and update, in the same transaction.
async fn tidy(_ctx: Ctx, mut note: Row) -> Result<Row> {
    let title: String = note.get("title")?;
    note.set("title", title.trim())?;
    Ok(note)
}

fn main() -> std::process::ExitCode {
    trestle::project()
        .intent_str("trestle.toml", include_str!("trestle.toml"))
        .migrations_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/notes_intent/migrations"
        ))
        .handler("notes::pin", pin)
        .hook("notes::tidy", tidy)
        .run()
}
