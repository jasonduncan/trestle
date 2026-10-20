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

/// SR-03: production boot refuses a non-local database without
/// `sslmode=verify-full` before it connects anywhere, so an unauthenticated
/// server never sees the credentials. A connection attempt shows up as
/// "connect to database" (the host does not resolve, or the sandbox denies it).
#[test]
fn production_refuses_weak_database_tls_before_connecting() {
    for (mode, refused) in [
        ("require", true),
        ("verify-ca", true),
        ("verify-full", false),
    ] {
        let mut command = if sandboxed() {
            let mut c = Command::new(SANDBOX);
            c.args(["-p", DENY_NETWORK, BIN]);
            c
        } else {
            Command::new(BIN)
        };
        let out = command
            .arg("serve")
            .current_dir(empty_dir())
            .env_clear()
            .env("RUST_LOG", "off")
            .env("TRESTLE_ENV", "production")
            .env(
                "DATABASE_URL",
                format!("postgres://pilot@db.invalid/pilot?sslmode={mode}"),
            )
            .env("TRESTLE_PASSWORD_LOGIN", "true")
            .env("TRESTLE_TRUST_PEER_ADDRESS", "true")
            .env("TRESTLE_CURSOR_KEY", "c".repeat(32))
            .env("TRESTLE_SESSION_KEY", "s".repeat(32))
            .env("TRESTLE_PUBLIC_ORIGIN", "https://app.example.com")
            .env("TRESTLE_BIND", "127.0.0.1:0")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{mode}: {stderr}");
        assert_eq!(
            stderr.contains("must use sslmode=verify-full"),
            refused,
            "{mode}: {stderr}"
        );
        assert_eq!(
            stderr.contains("connect to database"),
            !refused,
            "{mode}: {stderr}"
        );
    }
}
