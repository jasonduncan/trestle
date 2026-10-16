//! F2 digest compatibility: runtime loading must give every committed
//! migration exactly the digest the old build.rs-generated code gave it, or
//! databases that applied them would look tampered with.
//!
//! `golden/digests.json` was printed by the build.rs version (c23638d) and
//! `golden/legacy_ledger.csv` is the `trestle_migrations` ledger that
//! version wrote into a real database. Never regenerate them from new code.

use std::collections::BTreeMap;
use std::path::Path;

use trestle_historical::load;
use trestle_historical::testing::TestDatabase;
use trestle_historical::{MigrationSet, PlanState, migrate, plan};

/// Whether a golden key belongs to an app that has since moved out of
/// pilot (its migrations live in another project now).
fn moved(key: &str) -> bool {
    pilot_migrations::FOREIGN
        .iter()
        .any(|app| key.starts_with(&format!("{app}.")))
}

fn golden() -> BTreeMap<String, String> {
    let all: BTreeMap<String, String> =
        serde_json::from_str(include_str!("golden/digests.json")).unwrap();
    all.into_iter().filter(|(k, _)| !moved(k)).collect()
}

fn legacy_ledger() -> Vec<(String, String, String, String)> {
    include_str!("golden/legacy_ledger.csv")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            (f[0].into(), f[1].into(), f[2].into(), f[3].into())
        })
        .collect()
}

fn digests(set: &MigrationSet) -> BTreeMap<String, String> {
    set.iter()
        .map(|m| (m.key().to_string(), m.digest()))
        .collect()
}

fn on_disk() -> MigrationSet {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    load::history_from_dir(&dir, &pilot_migrations::data::steps())
        .unwrap()
        .with_foreign_apps(pilot_migrations::FOREIGN.iter().copied())
        .unwrap()
}

#[test]
fn embedded_history_has_the_golden_digests() {
    assert_eq!(digests(&pilot_migrations::history().unwrap()), golden());
}

#[test]
fn directory_history_has_the_golden_digests() {
    assert_eq!(digests(&on_disk()), golden());
}

#[test]
fn the_golden_files_agree_with_each_other() {
    let from_ledger: BTreeMap<String, String> = legacy_ledger()
        .into_iter()
        .map(|(app, id, digest, _)| (format!("{app}.{id}"), digest))
        .filter(|(k, _)| !moved(k))
        .collect();
    assert_eq!(from_ledger, golden());
}

#[test]
fn embedded_files_match_the_directory() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    assert_eq!(
        pilot_migrations::files(),
        load::read_dir(&dir).unwrap(),
        "the embedded copy is stale; touch src/lib.rs and rebuild"
    );
}

#[test]
fn order_is_unchanged() {
    let order: Vec<String> = pilot_migrations::history()
        .unwrap()
        .iter()
        .map(|m| m.key().to_string())
        .collect();
    let golden_order: Vec<&str> = include_str!("golden/digests.json")
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"')?.split('"').next())
        .filter(|k| !moved(k))
        .collect();
    assert_eq!(order, golden_order);
}

/// A database whose ledger was written by the old code accepts the new
/// loader: everything is applied, nothing runs, nothing is "changed".
#[tokio::test]
async fn a_legacy_ledger_accepts_the_runtime_loader() {
    let db = TestDatabase::create().await;
    let history = on_disk();
    migrate(db.options(), &history).await.unwrap();

    let mut conn = db.connect().await;
    // Replace the ledger with the one the old code wrote.
    sqlx::query("DELETE FROM trestle_migrations")
        .execute(&mut conn)
        .await
        .unwrap();
    for (app, id, digest, status) in legacy_ledger() {
        sqlx::query(
            "INSERT INTO trestle_migrations (app, id, digest, status, attempts, finished_at) \
             VALUES ($1, $2, $3, $4, 1, now())",
        )
        .bind(app)
        .bind(id)
        .bind(digest)
        .bind(status)
        .execute(&mut conn)
        .await
        .unwrap();
    }

    for set in [history.clone(), pilot_migrations::history().unwrap()] {
        let planned = plan(&mut conn, &set).await.unwrap();
        assert!(
            planned
                .entries
                .iter()
                .all(|e| e.state == PlanState::Applied),
            "{:?}",
            planned.entries
        );
        assert!(migrate(db.options(), &set).await.unwrap().is_empty());
    }
    drop(conn);
    db.drop().await;
}
