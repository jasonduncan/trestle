//! The schema fence (ADR-017): writes from builds older than the schema are
//! refused.
//!
//! A migration whose manifest says `"fence": true` changes the schema in a
//! way older code would get wrong without an error (a column's meaning or
//! unit, an invariant only code enforced). Applying it records it in
//! `trestle_schema_fence`, one row per app. From then on a build may write
//! only if it contains that app's newest fence migration:
//!
//! ```text
//! fence migration:  LOCK fence ACCESS EXCLUSIVE -> its SQL -> record -> commit
//! fenced write:     SELECT trestle_fence_v1(apps, known); BEGIN -> ... -> commit
//! ```
//!
//! The check and `BEGIN` travel in one message, so a fenced write costs no
//! extra round trip. The check holds `ACCESS SHARE` on the fence table
//! until the write commits, so a fence migration first waits for writes
//! already past the check, and writes arriving meanwhile wait for it. The
//! check's lookup takes a fresh snapshot after that wait (READ COMMITTED),
//! so a write that waited sees the fence the migration raised. A write the
//! fence refuses fails before `BEGIN` with SQLSTATE [`SQLSTATE`]: nothing
//! ran, and the connection is left idle.

use std::collections::BTreeSet;

use sqlx::PgConnection;

use crate::{Migration, MigrationSet};

/// One row per app that has applied a fence migration.
pub const TABLE: &str = "trestle_schema_fence";
/// Raised by the check when this build is fenced out.
pub const SQLSTATE: &str = "TRF01";

/// Created by every `migrate` run, like the ledger. The function is
/// versioned in its name: a build calls the version it was written for.
pub(crate) const CREATE: &str = r#"
CREATE TABLE IF NOT EXISTS trestle_schema_fence (
    app        text        PRIMARY KEY,
    migration  text        NOT NULL,
    generation integer     NOT NULL,
    raised_at  timestamptz NOT NULL DEFAULT now()
);
CREATE OR REPLACE FUNCTION trestle_fence_v1(apps text[], known text[]) RETURNS void
LANGUAGE plpgsql AS $fence$
DECLARE
    stale text;
BEGIN
    -- Waits for a fence migration in progress. The lookup below is a new
    -- statement, so (READ COMMITTED) it sees what that migration recorded.
    LOCK TABLE trestle_schema_fence IN ACCESS SHARE MODE;
    SELECT string_agg(f.app || '.' || f.migration, ', ' ORDER BY f.app) INTO stale
      FROM trestle_schema_fence f
     WHERE f.app = ANY(apps) AND NOT (f.app || '.' || f.migration = ANY(known));
    IF stale IS NOT NULL THEN
        RAISE EXCEPTION 'schema fenced by %', stale
            USING ERRCODE = 'TRF01',
                  HINT = 'This build does not contain these migrations; deploy one that does.';
    END IF;
END
$fence$;
"#;

/// What a build knows: the apps whose history it carries, and every fence
/// migration in that history. Foreign apps are not its apps, so their
/// fences do not apply to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildFence {
    apps: Vec<String>,
    known: Vec<String>,
}

impl BuildFence {
    /// `apps` are app IDs; `known` are fence migrations as `app.id`.
    pub fn new<A, K>(apps: A, known: K) -> Self
    where
        A: IntoIterator,
        A::Item: Into<String>,
        K: IntoIterator,
        K::Item: Into<String>,
    {
        let apps: BTreeSet<String> = apps.into_iter().map(Into::into).collect();
        let known: BTreeSet<String> = known.into_iter().map(Into::into).collect();
        Self {
            apps: apps.into_iter().collect(),
            known: known.into_iter().collect(),
        }
    }

    /// The fence of a build carrying `set`.
    pub fn of(set: &MigrationSet) -> Self {
        Self::new(
            set.iter().map(|m| m.app),
            set.iter()
                .filter(|m| m.options().fence)
                .map(|m| m.key().to_string()),
        )
    }

    pub fn apps(&self) -> &[String] {
        &self.apps
    }

    pub fn known(&self) -> &[String] {
        &self.known
    }

    /// A build without history has no app a fence could name.
    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// The check as one statement; errors with [`SQLSTATE`] when fenced.
    pub fn check_sql(&self) -> String {
        format!(
            "SELECT trestle_fence_v1({}, {})",
            array(&self.apps),
            array(&self.known)
        )
    }

    /// Check, then `BEGIN`, for `begin_with`. In one simple query the
    /// check runs in an implicit transaction that `BEGIN` turns into the
    /// write's own, so its lock lasts until the write ends; when the check
    /// fails, nothing after it runs and the implicit transaction rolls back.
    pub fn begin_sql(&self) -> String {
        format!("{}; BEGIN", self.check_sql())
    }
}

/// A text-array literal. IDs are ours, not request data; quotes are still
/// doubled (standard_conforming_strings is on by default since 9.1).
fn array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|item| format!("'{}'", item.replace('\'', "''")))
        .collect();
    format!("ARRAY[{}]::text[]", quoted.join(", "))
}

/// Whether `migrate` has created the fence (a ledger from an older
/// Trestle has not).
pub async fn installed(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT to_regprocedure('trestle_fence_v1(text[],text[])') IS NOT NULL")
        .fetch_one(conn)
        .await
}

/// One app's fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FenceRow {
    pub app: String,
    /// The newest fence migration applied for `app`.
    pub migration: String,
    /// How many fence migrations `app` has applied.
    pub generation: i32,
}

impl FenceRow {
    pub fn key(&self) -> String {
        format!("{}.{}", self.app, self.migration)
    }
}

/// Every app's fence; empty before the first fence migration, or before
/// `migrate` created the table.
pub async fn status(conn: &mut PgConnection) -> Result<Vec<FenceRow>, sqlx::Error> {
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('trestle_schema_fence') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !present {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String, i32)> =
        sqlx::query_as("SELECT app, migration, generation FROM trestle_schema_fence ORDER BY app")
            .fetch_all(conn)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(app, migration, generation)| FenceRow {
            app,
            migration,
            generation,
        })
        .collect())
}

/// The fence migrations that shut `build` out, as `app.id`; empty when it
/// may write. For startup and readiness: it reads without waiting.
pub async fn fenced_by(
    conn: &mut PgConnection,
    build: &BuildFence,
) -> Result<Vec<String>, sqlx::Error> {
    Ok(status(conn)
        .await?
        .into_iter()
        .filter(|row| build.apps.contains(&row.app) && !build.known.contains(&row.key()))
        .map(|row| row.key())
        .collect())
}

/// First statement of a fence migration's transaction: waits for writes
/// already past the check and holds back new ones until commit.
pub(crate) async fn lock(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("LOCK TABLE trestle_schema_fence IN ACCESS EXCLUSIVE MODE")
        .execute(conn)
        .await?;
    Ok(())
}

/// Records `migration` as its app's fence, under [`lock`]. Recording the
/// same migration again (a resumed non-atomic run) changes nothing.
pub(crate) async fn record(
    conn: &mut PgConnection,
    migration: &Migration,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO trestle_schema_fence (app, migration, generation) VALUES ($1, $2, 1)
         ON CONFLICT (app) DO UPDATE SET
             migration = excluded.migration,
             generation = trestle_schema_fence.generation + 1,
             raised_at = now()
         WHERE trestle_schema_fence.migration <> excluded.migration",
    )
    .bind(migration.app)
    .bind(migration.id)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_quote_ids_and_sort_them() {
        let build = BuildFence::new(["lessons", "audit", "lessons"], ["lessons.0005_o'brien"]);
        assert_eq!(build.apps(), ["audit", "lessons"]);
        assert_eq!(
            build.begin_sql(),
            "SELECT trestle_fence_v1(ARRAY['audit', 'lessons']::text[], \
             ARRAY['lessons.0005_o''brien']::text[]); BEGIN"
        );
        assert_eq!(
            BuildFence::new(Vec::<String>::new(), Vec::<String>::new()).check_sql(),
            "SELECT trestle_fence_v1(ARRAY[]::text[], ARRAY[]::text[])"
        );
    }
}
