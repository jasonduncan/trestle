//! The installed `trestle` command (design §3.1).
//!
//! Outside a project it scaffolds one (`trestle new`). Inside one, every
//! other command is handed to the project's own binary, like Django's
//! `manage.py`: `cargo run --quiet -- <args>`, or a built binary given by
//! `--bin PATH` / `TRESTLE_BIN` (what production and the reconciler use).

#![forbid(unsafe_code)]

pub mod scaffold;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Exit code for usage and configuration errors (design §5).
pub const USAGE: u8 = 2;

pub const HELP: &str = "\
trestle: scaffold a Trestle project, or run a project's command line.

Usage:
  trestle new NAME [--path DIR] [--git URL] [--tag TAG | --trestle-path DIR]
                   (default: the crates.io release of trestle-framework)
  trestle [--bin PATH] <command> [args...]    (inside a project)

Inside a project (a directory with trestle.toml, or below one) every command
is passed to the project binary: `cargo run --quiet -- <command>`, or the
built binary from --bin / TRESTLE_BIN. Run `trestle help` there for the
project's commands (serve, check, migrate, makemigrations, openapi, ...).
";

/// What the wrapper will do with its arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    New(Vec<OsString>),
    Help,
    /// Run `program args` in `dir`.
    Delegate {
        dir: PathBuf,
        program: OsString,
        args: Vec<OsString>,
    },
    NotInProject,
}

/// Decides what to do. `bin_env` is `TRESTLE_BIN`.
pub fn plan(args: Vec<OsString>, cwd: &Path, bin_env: Option<OsString>) -> Plan {
    let mut args = args;
    let mut bin = bin_env.filter(|b| !b.is_empty());
    if args.first().is_some_and(|a| a == "--bin") && args.len() >= 2 {
        bin = Some(args.remove(1));
        args.remove(0);
    } else if let Some(v) = args
        .first()
        .and_then(|a| a.to_str())
        .and_then(|a| a.strip_prefix("--bin="))
    {
        bin = Some(v.into());
        args.remove(0);
    }
    if args.first().is_some_and(|a| a == "new") {
        return Plan::New(args.split_off(1));
    }
    let root = project_root(cwd);
    match (root, bin) {
        (_, Some(program)) => Plan::Delegate {
            dir: cwd.to_path_buf(),
            program,
            args,
        },
        (Some(dir), None) => {
            let mut cargo = vec!["run".into(), "--quiet".into(), "--".into()];
            cargo.extend(args);
            Plan::Delegate {
                dir,
                program: std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
                args: cargo,
            }
        }
        (None, None)
            if args.is_empty() || args[0] == "help" || args[0] == "--help" || args[0] == "-h" =>
        {
            Plan::Help
        }
        (None, None) => Plan::NotInProject,
    }
}

/// The nearest directory at or above `start` holding both `trestle.toml`
/// and `Cargo.toml`.
pub fn project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join("trestle.toml").is_file() && d.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

pub fn main_with(args: Vec<OsString>) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: current directory: {e}");
            return ExitCode::from(USAGE);
        }
    };
    match plan(args, &cwd, std::env::var_os("TRESTLE_BIN")) {
        Plan::Help => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Plan::NotInProject => {
            eprintln!(
                "error: not inside a Trestle project (no trestle.toml here or above).\n\
                 Create one with `trestle new NAME`, or pass --bin PATH to a built project binary."
            );
            ExitCode::from(USAGE)
        }
        Plan::New(args) => scaffold::run(args, &cwd),
        Plan::Delegate { dir, program, args } => {
            match Command::new(&program)
                .args(&args)
                .current_dir(&dir)
                .status()
            {
                Ok(status) => match status.code() {
                    Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
                    None => ExitCode::FAILURE,
                },
                Err(e) => {
                    eprintln!(
                        "error: could not run {}: {e}",
                        Path::new(&program).display()
                    );
                    ExitCode::from(USAGE)
                }
            }
        }
    }
}
