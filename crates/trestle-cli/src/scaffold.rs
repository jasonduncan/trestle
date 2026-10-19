//! `trestle new`: writes the project template (design §4). Touches no
//! network; the first `cargo build` fetches dependencies.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use include_dir::{Dir, include_dir};

static ADMIN_TEMPLATE: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/template-admin");

static TEMPLATE: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/template");

/// The Trestle release this wrapper scaffolds against by default.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The minimum Rust version new projects declare (Trestle's own).
pub const RUST_VERSION: &str = env!("CARGO_PKG_RUST_VERSION");
/// The published package; projects import it as `trestle`.
pub const PACKAGE: &str = "trestle-framework";
pub const DEFAULT_GIT: &str = "https://github.com/jasonduncan/trestle";

#[derive(Parser, Debug)]
#[command(name = "trestle new", about = "Create a new Trestle project")]
pub struct NewArgs {
    /// Project name: lowercase letters, digits, `-` and `_`.
    pub name: String,
    /// Add the separate React admin host and an authorized workspace directory.
    #[arg(long)]
    pub admin: bool,
    /// Directory to create (default: ./NAME).
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Depend on a git repository instead of the crates.io release.
    #[arg(long, conflicts_with = "trestle_path")]
    pub git: Option<String>,
    /// Release tag in that repository (default repository if --git is not
    /// given; default tag: this wrapper's version).
    #[arg(long, alias = "git-tag", conflicts_with = "trestle_path")]
    pub tag: Option<String>,
    /// Depend on a local Trestle checkout instead (for Trestle's own CI).
    #[arg(long)]
    pub trestle_path: Option<PathBuf>,
}

pub fn run(args: Vec<OsString>, cwd: &Path) -> ExitCode {
    let mut argv = vec![OsString::from("trestle new")];
    argv.extend(args);
    let args = match NewArgs::try_parse_from(argv) {
        Ok(a) => a,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { crate::USAGE } else { 0 });
        }
    };
    let dir = cwd.join(
        args.path
            .clone()
            .unwrap_or_else(|| PathBuf::from(&args.name)),
    );
    match create(&args, &dir) {
        Ok(()) => {
            println!(
                "Created {name} in {dir}.\n\n\
                 Next:\n  cd {dir}\n  createdb {name}\n  trestle migrate\n  trestle check\n  cargo test\n\n\
                 .env holds development keys and a dev token (never commit it).",
                name = args.name,
                dir = dir.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(crate::USAGE)
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The `trestle = ...` dependency value.
pub fn dependency(args: &NewArgs, features: &str) -> String {
    let source = match (&args.trestle_path, &args.git, &args.tag) {
        (Some(p), _, _) => format!(
            "path = {:?}",
            p.join("crates/trestle").display().to_string()
        ),
        (None, None, None) => {
            let short = VERSION.rsplit_once('.').map_or(VERSION, |(a, _)| a);
            format!("version = {short:?}")
        }
        (None, git, tag) => {
            let git = git.as_deref().unwrap_or(DEFAULT_GIT);
            let tag = tag.clone().unwrap_or_else(|| format!("v{VERSION}"));
            format!("git = {git:?}, tag = {tag:?}")
        }
    };
    format!("{{ package = {PACKAGE:?}, {source}{features} }}")
}

/// Writes the template into `dir`, which must not exist or be empty.
pub fn create(args: &NewArgs, dir: &Path) -> Result<(), String> {
    if !valid_name(&args.name) {
        return Err(format!(
            "project name {:?} must start with a lowercase letter and use only a-z, 0-9, - and _",
            args.name
        ));
    }
    if dir.exists()
        && std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .next()
            .is_some()
    {
        return Err(format!("{} already exists and is not empty", dir.display()));
    }
    let short_version = VERSION.rsplit_once('.').map_or(VERSION, |(a, _)| a);
    let vars = [
        ("{{name}}", args.name.clone()),
        ("{{trestle_version}}", short_version.to_owned()),
        ("{{rust_version}}", RUST_VERSION.to_owned()),
        ("{{trestle_dep}}", dependency(args, "")),
        (
            "{{trestle_dep_testing}}",
            dependency(args, ", features = [\"testing\"]"),
        ),
    ];
    write_dir(&TEMPLATE, dir, &vars)?;
    let write = |name: &str, text: String| {
        std::fs::write(dir.join(name), text).map_err(|e| format!("{name}: {e}"))
    };
    if args.admin {
        write_dir(&ADMIN_TEMPLATE, dir, &vars)?;
        let manifest =
            std::fs::read_to_string(dir.join("Cargo.toml")).map_err(|e| e.to_string())?;
        write("Cargo.toml", manifest.replace("[dependencies]", "[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nsqlx = { version = \"0.8\", default-features = false, features = [\"runtime-tokio\", \"postgres\", \"uuid\"] }"))?;
        let intent =
            std::fs::read_to_string(dir.join("trestle.toml")).map_err(|e| e.to_string())?;
        write(
            "trestle.toml",
            intent.replace(
                "member = [\"notes.read\", \"notes.write\"]",
                "member = [\"notes.read\", \"notes.write\", \"notes.admin\"]",
            ) + "\n[[admin]]\nmodel = \"notes.note\"\npermission = \"notes.admin\"\ncolumns = [\"title\", \"pinned\"]\n",
        )?;
    }
    write(".env", dev_env(&args.name))?;
    Ok(())
}

fn write_dir(template: &Dir<'_>, dir: &Path, vars: &[(&str, String)]) -> Result<(), String> {
    for file in template.files() {
        let rel = file.path();
        // Dotfiles are stored without the dot so packaging keeps them, and
        // the manifest as `Cargo.toml.in`: a file named `Cargo.toml` here
        // would be read by cargo as a package when projects depend on
        // trestle by git, and its placeholders do not parse.
        let name = match rel.file_name().and_then(|n| n.to_str()) {
            Some("Cargo.toml.in") => rel.with_file_name("Cargo.toml"),
            Some("gitignore") => rel.with_file_name(".gitignore"),
            Some("env.example") => rel.with_file_name(".env.example"),
            _ => rel.to_path_buf(),
        };
        let target = dir.join(name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut text = file
            .contents_utf8()
            .ok_or_else(|| format!("template file {} is not UTF-8", rel.display()))?
            .to_owned();
        for (k, v) in vars {
            text = text.replace(k, v);
        }
        std::fs::write(&target, text).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    for sub in template.dirs() {
        write_dir(sub, dir, vars)?;
    }
    Ok(())
}

fn secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("operating system randomness is available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A local `.env` with fresh development keys. Written to disk only; the
/// wrapper never prints it.
fn dev_env(name: &str) -> String {
    let actor = uuid::Uuid::new_v4();
    format!(
        "# Development only. Generated by `trestle new`; never commit this file.\n\
         DATABASE_URL=postgres://localhost/{name}\n\
         TRESTLE_ENV=development\n\
         TRESTLE_PUBLIC_ORIGIN=http://localhost:8000\n\
         TRESTLE_DEV_TOKENS={token}={actor}\n\
         TRESTLE_SESSION_KEY={session}\n\
         TRESTLE_CURSOR_KEY={cursor}\n",
        token = secret(),
        session = secret(),
        cursor = secret(),
    )
}
