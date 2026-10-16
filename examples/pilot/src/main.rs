//! `pilot`: the project binary is the CLI (Trestle's `manage.py` pattern).
//! Built-in commands come from `trestle`; pilot adds its own in
//! `pilot::commands`.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    pilot::cli().run()
}
