//! F4: the `trestle` wrapper and the `new` template.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::json;
use trestle::testing::TestApp;
use trestle_cli::scaffold::{self, NewArgs};
use trestle_cli::{Plan, plan};

const TEMPLATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/template");

fn args(a: &[&str]) -> Vec<OsString> {
    a.iter().map(OsString::from).collect()
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trestle-cli-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn outside_a_project_only_new_and_help_work() {
    let dir = tempdir("outside");
    assert_eq!(
        plan(args(&["new", "x"]), &dir, None),
        Plan::New(args(&["x"]))
    );
    assert_eq!(plan(args(&[]), &dir, None), Plan::Help);
    assert_eq!(plan(args(&["migrate"]), &dir, None), Plan::NotInProject);
}

#[test]
fn inside_a_project_commands_go_to_cargo_run_from_the_root() {
    let root = tempdir("inside");
    std::fs::write(root.join("trestle.toml"), "").unwrap();
    std::fs::write(root.join("Cargo.toml"), "").unwrap();
    let nested = root.join("src/deep");
    std::fs::create_dir_all(&nested).unwrap();
    let Plan::Delegate { dir, args: a, .. } = plan(args(&["migrate", "--plan"]), &nested, None)
    else {
        panic!("expected delegation")
    };
    assert_eq!(dir, root);
    assert_eq!(a, args(&["run", "--quiet", "--", "migrate", "--plan"]));
}

#[test]
fn a_built_binary_is_used_when_given() {
    let dir = tempdir("bin");
    let expected = Plan::Delegate {
        dir: dir.clone(),
        program: "/srv/app".into(),
        args: args(&["verify"]),
    };
    assert_eq!(
        plan(args(&["--bin", "/srv/app", "verify"]), &dir, None),
        expected
    );
    assert_eq!(
        plan(args(&["--bin=/srv/app", "verify"]), &dir, None),
        expected
    );
    assert_eq!(
        plan(args(&["verify"]), &dir, Some("/srv/app".into())),
        expected
    );
}

fn new_args(name: &str) -> NewArgs {
    NewArgs {
        admin: false,
        name: name.into(),
        path: None,
        git: None,
        tag: None,
        trestle_path: None,
    }
}

#[test]
fn new_writes_the_template() {
    let dir = tempdir("new").join("demo");
    scaffold::create(&new_args("demo"), &dir).unwrap();
    for f in [
        "Cargo.toml",
        "trestle.toml",
        ".env.example",
        ".env",
        ".gitignore",
        "README.md",
        "src/main.rs",
        "tests/api.rs",
        "migrations/notes/0001_initial.json",
        "migrations/workspace/0001_initial.sql",
    ] {
        assert!(dir.join(f).is_file(), "{f} missing");
    }
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
    let short = scaffold::VERSION.rsplit_once('.').unwrap().0;
    assert!(read("Cargo.toml").contains(&format!(
        "trestle = {{ package = \"trestle-framework\", version = \"{short}\" }}"
    )));
    assert!(read("Cargo.toml").contains(&format!("rust-version = \"{}\"", scaffold::RUST_VERSION)));
    assert!(
        !dir.join("rust-toolchain.toml").exists(),
        "no toolchain pin"
    );
    assert!(read("trestle.toml").contains("name = \"demo\""));
    assert!(!read("trestle.toml").contains("{{"));
    assert!(read(".gitignore").contains(".env"));
    let env = read(".env");
    for key in ["TRESTLE_SESSION_KEY=", "TRESTLE_CURSOR_KEY="] {
        let value = env.lines().find_map(|l| l.strip_prefix(key)).unwrap();
        assert_eq!(value.len(), 64, "{key} has 32 random bytes");
    }
    assert_ne!(read(".env"), {
        let other = dir.with_file_name("demo2");
        scaffold::create(&new_args("demo2"), &other).unwrap();
        std::fs::read_to_string(other.join(".env")).unwrap()
    });

    // Refuses to overwrite, and bad names.
    assert!(scaffold::create(&new_args("demo"), &dir).is_err());
    assert!(scaffold::create(&new_args("Demo"), &dir.with_file_name("x")).is_err());
}

#[test]
fn a_local_checkout_can_be_the_dependency() {
    let mut a = new_args("demo");
    a.tag = None;
    a.trestle_path = Some("/src/trestle".into());
    assert_eq!(
        scaffold::dependency(&a, ""),
        "{ package = \"trestle-framework\", path = \"/src/trestle/crates/trestle\" }"
    );
}

#[test]
fn git_and_tag_override_the_registry() {
    let mut a = new_args("demo");
    a.tag = Some("v0.2.0".into());
    assert_eq!(
        scaffold::dependency(&a, ""),
        "{ package = \"trestle-framework\", git = \"https://github.com/jasonduncan/trestle\", tag = \"v0.2.0\" }"
    );
    a.git = Some("https://example.com/fork".into());
    assert!(scaffold::dependency(&a, "").contains("git = \"https://example.com/fork\""));
}

fn template_project() -> trestle::Project {
    let text = std::fs::read_to_string(Path::new(TEMPLATE).join("trestle.toml"))
        .unwrap()
        .replace("{{name}}", "demo")
        .replace("{{trestle_version}}", "0.0");
    trestle::project()
        .intent_str("trestle.toml", &text)
        .migrations_dir(Path::new(TEMPLATE).join("migrations"))
}

#[tokio::test]
async fn the_template_passes_check() {
    let project = template_project();
    let matches = project.command().get_matches_from(["demo", "check"]);
    project.run_matches(&matches).await.unwrap();
}

/// The same request as the template's own tests/api.rs.
#[tokio::test]
async fn the_template_serves_its_api() {
    const ALICE: &str = "00000000-0000-4000-8000-00000000a11c";
    let app = TestApp::start(template_project(), &format!("alice-dev-token={ALICE}")).await;
    let workspace = app.tenant("Test").await;
    app.grant(workspace, ALICE.parse().unwrap(), "member").await;
    let res = app
        .post("/api/v1/notes", json!({ "title": "Hello" }))
        .token("alice-dev-token")
        .tenant(workspace)
        .send()
        .await;
    assert_eq!(res.status, 201, "{}", res.body);
    assert_eq!(res.etag(), Some("\"notes.note:1\""));
    app.finish().await;
}

/// Projects depend on trestle by git, and cargo reads every `Cargo.toml` in
/// the repository; a template manifest with placeholders must not be one.
#[test]
fn the_template_has_no_cargo_toml() {
    fn walk(dir: &std::path::Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            assert_ne!(
                path.file_name().unwrap(),
                "Cargo.toml",
                "{}",
                path.display()
            );
            if path.is_dir() {
                walk(&path);
            }
        }
    }
    walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("template"));
}
