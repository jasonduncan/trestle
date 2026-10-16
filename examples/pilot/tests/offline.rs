//! M1 (design doc §18, I09): registry checks and exports run with networking
//! disabled. On macOS the binary runs under `sandbox-exec` with all network
//! access denied, and a control command proves the sandbox really blocks it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pilot");
const SANDBOX: &str = "/usr/bin/sandbox-exec";
const DENY_NETWORK: &str = "(version 1)(allow default)(deny network*)";

fn sandboxed() -> bool {
    cfg!(target_os = "macos") && Path::new(SANDBOX).exists()
}

/// A directory with no `.env`, so nothing is configured implicitly.
fn empty_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trestle-offline-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) -> Output {
    let mut command = if sandboxed() {
        let mut c = Command::new(SANDBOX);
        c.args(["-p", DENY_NETWORK, BIN]);
        c
    } else {
        Command::new(BIN)
    };
    command
        .args(args)
        .current_dir(empty_dir())
        .env("DATABASE_URL", "postgres://localhost/trestle_offline_check")
        .env("RUST_LOG", "off")
        .output()
        .unwrap()
}

#[test]
fn checks_and_exports_need_no_database_or_network() {
    for args in [
        &["check"][..],
        &["inspect", "apps"],
        &["inspect", "routes"],
        &["inspect", "operation", "lessons.create"],
        &["contracts", "export"],
        &["interactions", "export"],
        &["openapi", "export"],
        &["makemigrations", "--check"],
        &["schema", "show"],
        &["schema", "show", "--history"],
    ] {
        let out = run(args);
        assert!(
            out.status.success(),
            "pilot {} failed offline: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.stdout.is_empty());
    }

    let openapi = run(&["openapi", "export"]).stdout;
    let parsed: serde_json::Value = serde_json::from_slice(&openapi).unwrap();
    assert_eq!(parsed["openapi"], "3.1.1");
    assert_eq!(
        openapi,
        run(&["openapi", "export"]).stdout,
        "exports are deterministic"
    );

    if sandboxed() {
        // Control: something that does need the network fails in the same sandbox.
        let out = run(&["migrate", "--plan"]);
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
