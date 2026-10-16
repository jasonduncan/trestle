//! F1: the `trestle` facade. Apps without a type parameter, the default
//! state, the project command line, and project commands.

use axum::body::Body;
use clap::Subcommand;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;
use trestle::core::identity::DevTokenIdentity;
use trestle::historical::testing::TestDatabase as TestDb;
use trestle::prelude::*;
use trestle::{CommandContext, Registry};

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

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/migrations");

/// Notes with its committed migrations read at runtime from the fixtures.
fn notes() -> trestle::Project {
    trestle::project()
        .install(Notes)
        .named("Notes", "1.2.3")
        .migrations_dir(FIXTURES)
}

#[test]
fn an_app_on_the_default_state_assembles() {
    let registry: Registry<State> = notes().assemble().unwrap();
    assert_eq!(registry.name(), "Notes");
    assert_eq!(registry.version(), "1.2.3");
    assert_eq!(
        registry.apps().iter().map(|a| a.id).collect::<Vec<_>>(),
        ["notes"]
    );
    assert!(registry.desired_schema().model("notes.note").is_some());
}

#[test]
fn assembly_problems_are_reported() {
    struct Broken;
    impl App for Broken {
        fn id(&self) -> &'static str {
            "broken"
        }
        fn register(&self, app: &mut AppBuilder) {
            app.requires("missing");
        }
    }
    let err = trestle::project()
        .install(Broken)
        .assemble()
        .err()
        .expect("unknown dependency is an assembly error");
    assert!(err.to_string().contains("missing"), "{err}");
}

#[test]
fn the_command_line_has_every_built_in_command() {
    let cmd = notes().command();
    let names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
    for expected in [
        "serve",
        "identity",
        "account",
        "mail",
        "migrate",
        "check",
        "inspect",
        "contracts",
        "interactions",
        "openapi",
        "makemigrations",
        "sdk",
        "schema",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    cmd.debug_assert();
}

#[derive(Subcommand)]
enum Extra {
    /// A project command.
    Hello {
        #[arg(long)]
        name: String,
    },
    /// Replaces the built-in `mail`.
    Mail,
}

async fn extra(command: Extra, ctx: CommandContext<State>) -> trestle::Result<(), String> {
    match command {
        Extra::Hello { name } => {
            assert_eq!(ctx.name, "Notes");
            assert!(ctx.registry.desired_schema().model("notes.note").is_some());
            if name == "fail" {
                return Err("asked to fail".into());
            }
            Ok(())
        }
        Extra::Mail => Ok(()),
    }
}

#[test]
fn project_commands_are_added_and_replace_built_ins() {
    let project = notes().commands(extra);
    let cmd = project.command();
    cmd.clone().debug_assert();
    let names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
    assert!(names.contains(&"hello"));
    assert_eq!(names.iter().filter(|n| **n == "mail").count(), 1);
    assert!(names.contains(&"check"));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let run = |args: &[&str]| {
        let matches = notes()
            .commands(extra)
            .command()
            .try_get_matches_from(args)
            .unwrap();
        rt.block_on(notes().commands(extra).run_matches(&matches))
    };
    run(&["notes", "hello", "--name", "x"]).unwrap();
    assert_eq!(
        run(&["notes", "hello", "--name", "fail"]).unwrap_err(),
        "asked to fail"
    );
    run(&["notes", "mail"]).unwrap();
    run(&["notes", "check"]).unwrap();
}

#[test]
fn offline_commands_run_without_a_database() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    for args in [
        &["notes", "check"][..],
        &["notes", "inspect", "apps"],
        &["notes", "openapi", "export"],
        &["notes", "contracts", "export"],
        &["notes", "schema", "show"],
        &["notes", "schema", "show", "--history"],
    ] {
        let matches = notes().command().try_get_matches_from(args).unwrap();
        rt.block_on(notes().run_matches(&matches))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
}

#[test]
fn makemigrations_check_reports_pending_models_and_writes_nothing() {
    let dir = std::env::temp_dir().join(format!("trestle-f1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let project = || {
        trestle::project()
            .install(Notes)
            .named("Notes", "1.2.3")
            .migrations_dir(&dir)
    };
    let matches = project()
        .command()
        .try_get_matches_from(["notes", "makemigrations", "--check"])
        .unwrap();
    let err = rt.block_on(project().run_matches(&matches)).unwrap_err();
    assert!(
        err.contains("models differ from committed history"),
        "{err}"
    );
    assert!(!dir.exists(), "--check writes nothing");

    let matches = project()
        .command()
        .try_get_matches_from(["notes", "makemigrations", "--name", "initial"])
        .unwrap();
    rt.block_on(project().run_matches(&matches)).unwrap();
    let written: Vec<_> = std::fs::read_dir(dir.join("notes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(written.iter().any(|f| f.ends_with(".sql")), "{written:?}");
    assert!(written.iter().any(|f| f.ends_with(".json")), "{written:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn usage_errors_are_reported_by_the_parser() {
    let err = notes()
        .command()
        .try_get_matches_from(["notes", "no-such-command"])
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

#[tokio::test]
async fn the_default_state_serves_the_registry() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    let registry = notes().assemble().unwrap();
    let identity = Arc::new(
        DevTokenIdentity::parse("test-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap(),
    );
    let app = trestle::router_with(
        &registry,
        State::new(pool.clone(), identity),
        Default::default(),
    );
    let response = app
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    pool.close().await;
    db.drop().await;
}
