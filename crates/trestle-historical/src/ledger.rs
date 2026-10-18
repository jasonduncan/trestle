//! The applied-history ledger (`trestle_migrations`).

use sqlx::PgConnection;

use crate::{Migration, MigrationRef};

const CREATE_LEDGER: &str = "
CREATE TABLE IF NOT EXISTS trestle_migrations (
    app         text        NOT NULL,
    id          text        NOT NULL,
    digest      text        NOT NULL,
    status      text        NOT NULL CHECK (status IN ('running', 'applied', 'failed', 'partial')),
    attempts    integer     NOT NULL DEFAULT 0,
    step        integer     NOT NULL DEFAULT 0,
    backup_ref  text,
    started_at  timestamptz,
    finished_at timestamptz,
    last_error  text,
    PRIMARY KEY (app, id)
)";

/// Ledgers created before non-atomic migrations lack `step` and the
/// `partial` status. One transaction; the ledger is small.
const UPGRADE_LEDGER: &str = "
ALTER TABLE trestle_migrations ADD COLUMN IF NOT EXISTS step integer NOT NULL DEFAULT 0;
ALTER TABLE trestle_migrations ADD COLUMN IF NOT EXISTS backup_ref text;
ALTER TABLE trestle_migrations DROP CONSTRAINT IF EXISTS trestle_migrations_status_check;
ALTER TABLE trestle_migrations ADD CONSTRAINT trestle_migrations_status_check
    CHECK (status IN ('running', 'applied', 'failed', 'partial'));
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerStatus {
    /// An attempt started. For a transactional migration this means its
    /// transaction never committed (the `applied` mark commits with the DDL).
    Running,
    Applied,
    /// The attempt errored and its transaction rolled back.
    Failed,
    /// A non-atomic migration stopped after some steps; `step` says where
    /// to resume.
    Partial,
}

impl LedgerStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "applied" => Some(Self::Applied),
            "failed" => Some(Self::Failed),
            "partial" => Some(Self::Partial),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LedgerRow {
    pub app: String,
    pub id: String,
    pub digest: String,
    pub status: LedgerStatus,
    pub attempts: i32,
    /// The next step of a non-atomic migration (0-based).
    pub step: i32,
}

pub(crate) async fn ensure(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    use sqlx::{Connection, Executor};
    sqlx::query(CREATE_LEDGER).execute(&mut *conn).await?;
    let current: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = 'trestle_migrations'
           AND column_name = 'backup_ref')",
    )
    .fetch_one(&mut *conn)
    .await?;
    if !current {
        let mut tx = conn.begin().await?;
        (&mut *tx).execute(UPGRADE_LEDGER).await?;
        tx.commit().await?;
    }
    // Batched data steps (ADR-020) record where they stopped.
    let checkpointed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = 'trestle_migrations'
           AND column_name = 'checkpoint')",
    )
    .fetch_one(&mut *conn)
    .await?;
    if !checkpointed {
        (&mut *conn)
            .execute("ALTER TABLE trestle_migrations ADD COLUMN IF NOT EXISTS checkpoint text")
            .await?;
    }
    (&mut *conn).execute(crate::fence::CREATE).await?;
    Ok(())
}

pub async fn exists(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass('trestle_migrations') IS NOT NULL")
        .fetch_one(conn)
        .await
}

/// All ledger rows; empty when the ledger has not been created yet.
pub async fn load(conn: &mut PgConnection) -> Result<Vec<LedgerRow>, sqlx::Error> {
    if !exists(conn).await? {
        return Ok(Vec::new());
    }
    let has_step: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = 'trestle_migrations'
           AND column_name = 'step')",
    )
    .fetch_one(&mut *conn)
    .await?;
    let step = if has_step { "step" } else { "0" };
    let rows: Vec<(String, String, String, String, i32, i32)> = sqlx::query_as(&format!(
        "SELECT app, id, digest, status, attempts, {step} FROM trestle_migrations ORDER BY app, id"
    ))
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|(app, id, digest, status, attempts, step)| {
            let status = LedgerStatus::parse(&status).ok_or_else(|| {
                sqlx::Error::Protocol(format!("unknown ledger status {status:?}"))
            })?;
            Ok(LedgerRow {
                app,
                id,
                digest,
                status,
                attempts,
                step,
            })
        })
        .collect()
}

pub(crate) async fn mark_running(
    conn: &mut PgConnection,
    migration: &Migration,
    digest: &str,
    backup_ref: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO trestle_migrations (app, id, digest, status, attempts, started_at, backup_ref)
         VALUES ($1, $2, $3, 'running', 1, now(), $4)
         ON CONFLICT (app, id) DO UPDATE SET
             digest = excluded.digest,
             backup_ref = excluded.backup_ref,
             status = 'running',
             attempts = trestle_migrations.attempts + 1,
             started_at = now(),
             finished_at = NULL,
             last_error = NULL",
    )
    .bind(migration.app)
    .bind(migration.id)
    .bind(digest)
    .bind(backup_ref)
    .execute(conn)
    .await?;
    Ok(())
}

/// Records progress of a non-atomic migration: `step` is the next step to
/// run. Commits on its own (autocommit), before and after each step.
pub(crate) async fn mark_step(
    conn: &mut PgConnection,
    migration: &Migration,
    step: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE trestle_migrations SET step = $3 WHERE app = $1 AND id = $2")
        .bind(migration.app)
        .bind(migration.id)
        .bind(step)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn mark_partial(
    conn: &mut PgConnection,
    migration: &Migration,
    step: i32,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE trestle_migrations SET status = 'partial', step = $3, finished_at = now(),
             last_error = $4
         WHERE app = $1 AND id = $2",
    )
    .bind(migration.app)
    .bind(migration.id)
    .bind(step)
    .bind(message)
    .execute(conn)
    .await?;
    Ok(())
}

/// Where a batched data step stopped: the last committed batch's
/// checkpoint, as JSON.
pub(crate) async fn checkpoint(
    conn: &mut PgConnection,
    migration: &Migration,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let text: Option<Option<String>> =
        sqlx::query_scalar("SELECT checkpoint FROM trestle_migrations WHERE app = $1 AND id = $2")
            .bind(migration.app)
            .bind(migration.id)
            .fetch_optional(conn)
            .await?;
    Ok(text.flatten().and_then(|t| serde_json::from_str(&t).ok()))
}

/// Records a committed batch, inside that batch's transaction.
pub(crate) async fn mark_checkpoint(
    conn: &mut PgConnection,
    migration: &Migration,
    batches: i32,
    checkpoint: Option<&serde_json::Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE trestle_migrations SET step = $3, checkpoint = $4 WHERE app = $1 AND id = $2",
    )
    .bind(migration.app)
    .bind(migration.id)
    .bind(batches)
    .bind(checkpoint.map(|c| c.to_string()))
    .execute(conn)
    .await?;
    Ok(())
}

/// Runs inside the migration's own transaction.
pub(crate) async fn mark_applied(
    conn: &mut PgConnection,
    migration: &Migration,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE trestle_migrations SET status = 'applied', finished_at = now()
         WHERE app = $1 AND id = $2",
    )
    .bind(migration.app)
    .bind(migration.id)
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn mark_failed(
    conn: &mut PgConnection,
    migration: &Migration,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE trestle_migrations SET status = 'failed', finished_at = now(), last_error = $3
         WHERE app = $1 AND id = $2",
    )
    .bind(migration.app)
    .bind(migration.id)
    .bind(message)
    .execute(conn)
    .await?;
    Ok(())
}

/// Required migrations that are not applied. Web startup and readiness use
/// this to check schema capabilities; they never apply migrations (§14.4).
pub async fn missing(
    conn: &mut PgConnection,
    required: &[MigrationRef],
) -> Result<Vec<MigrationRef>, sqlx::Error> {
    let rows = load(conn).await?;
    Ok(required
        .iter()
        .filter(|needed| {
            !rows.iter().any(|row| {
                row.status == LedgerStatus::Applied && row.app == needed.app && row.id == needed.id
            })
        })
        .copied()
        .collect())
}
