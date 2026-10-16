//! The pilot's committed history replays into an empty database, and the
//! resulting catalog is exactly what the manifests say history built.

use trestle_historical::testing::TestDatabase;
use trestle_historical::verify::verify;
use trestle_historical::{migrate, missing};

#[tokio::test]
async fn history_replays_into_empty_database_and_matches_its_manifests() {
    let db = TestDatabase::create().await;
    let history = pilot_migrations::history().unwrap();

    let applied = migrate(db.options(), &history).await.unwrap();
    assert_eq!(applied.len(), 9);
    assert!(migrate(db.options(), &history).await.unwrap().is_empty());

    let mut conn = db.connect().await;
    let required = [
        pilot_migrations::SCHOOLS_0001,
        pilot_migrations::AUDIT_0001,
        pilot_migrations::LESSONS_0001,
        pilot_migrations::LESSONS_0002,
        pilot_migrations::LESSONS_0003,
        pilot_migrations::LESSONS_0004,
        pilot_migrations::UNITS_0001,
        pilot_migrations::IDENTITY_0001,
        pilot_migrations::IDENTITY_0002,
    ];
    assert!(missing(&mut conn, &required).await.unwrap().is_empty());

    let report = verify(&mut conn, &history.history().unwrap())
        .await
        .unwrap();
    assert!(report.ok(), "{:#?}", report.mismatches);
    assert!(
        report.unmanaged_tables.is_empty(),
        "{:?}",
        report.unmanaged_tables
    );
    drop(conn);
    db.drop().await;
}

#[test]
fn legacy_digests_are_unchanged() {
    // Format-1 manifests annotate existing SQL; the digest still covers the
    // SQL only, so databases that applied these migrations keep matching.
    let history = pilot_migrations::history().unwrap();
    let digest = |id: &str| {
        history
            .iter()
            .find(|m| format!("{}.{}", m.app, m.id) == id)
            .unwrap()
            .digest()
    };
    let recorded = [
        (
            "schools.0001_initial",
            include_str!("../migrations/schools/0001_initial.sql"),
        ),
        (
            "lessons.0002_subtitle",
            include_str!("../migrations/lessons/0002_subtitle.sql"),
        ),
    ];
    for (id, sql) in recorded {
        use sha2_like::digest_v1;
        assert_eq!(digest(id), digest_v1(sql), "{id}");
    }
}

mod sha2_like {
    /// The format-1 digest, recomputed independently of the crate.
    pub fn digest_v1(sql: &str) -> String {
        use std::process::Command;
        let mut input = b"trestle.migration/1\0".to_vec();
        input.extend_from_slice(sql.as_bytes());
        let path = std::env::temp_dir().join(format!("trestle-digest-{}", std::process::id()));
        std::fs::write(&path, &input).unwrap();
        let out = Command::new("shasum")
            .args(["-a", "256"])
            .arg(&path)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        String::from_utf8(out.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    }
}
