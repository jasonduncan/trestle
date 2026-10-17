//! The schema fence on the write path (ADR-017).
//!
//! `trestle serve` installs its build's [`BuildFence`] after checking that
//! the database has a fence and that it admits this build. From then on,
//! every write transaction the executor begins, every job claim and every
//! readiness probe checks it, and a build that a newer fence migration
//! shuts out answers 503 `schema_fenced` with nothing written. The check
//! rides in the same message as `BEGIN`; see
//! [`trestle_historical::fence`] for why that is race-free.
//!
//! Without an installed fence nothing is checked, which is how tests and
//! callers that never run `migrate` keep working. The fence covers writes
//! the executor runs; SQL a handler sends on its own pool does not pass
//! through it.

use std::sync::{Arc, PoisonError, RwLock};

use axum::http::{HeaderValue, header};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

pub use trestle_historical::fence::{BuildFence, FenceRow, SQLSTATE, fenced_by, installed, status};

use crate::Problem;

struct Installed {
    build: BuildFence,
    begin: String,
}

static INSTALLED: RwLock<Option<Arc<Installed>>> = RwLock::new(None);

/// Fences every write this process makes. One build per process, so this
/// is process-wide; installing again replaces the fence.
pub fn install(build: BuildFence) {
    let begin = build.begin_sql();
    *INSTALLED.write().unwrap_or_else(PoisonError::into_inner) =
        Some(Arc::new(Installed { build, begin }));
}

/// Stops checking (tests that install more than one build).
pub fn uninstall() {
    *INSTALLED.write().unwrap_or_else(PoisonError::into_inner) = None;
}

/// The installed fence, if any.
pub fn current() -> Option<BuildFence> {
    installed_fence().map(|f| f.build.clone())
}

fn installed_fence() -> Option<Arc<Installed>> {
    INSTALLED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Begins a write transaction, checking the installed fence in the same
/// round trip. A fenced build gets [`fenced`] and the connection goes back
/// to the pool idle.
pub async fn begin(pool: &PgPool) -> Result<Transaction<'static, Postgres>, Problem> {
    Ok(match installed_fence() {
        Some(fence) => pool.begin_with(fence.begin.clone()).await?,
        None => pool.begin().await?,
    })
}

/// For readiness: [`fenced`] when the installed fence shuts this build out.
/// Reads without waiting for a fence migration in progress.
pub async fn check(conn: &mut PgConnection) -> Result<(), Problem> {
    let Some(fence) = installed_fence() else {
        return Ok(());
    };
    let stale = fenced_by(conn, &fence.build).await?;
    if stale.is_empty() {
        Ok(())
    } else {
        tracing::warn!(fenced_by = ?stale, "schema fenced: this build may not write");
        Err(fenced())
    }
}

/// 503 `schema_fenced`: a newer release changed the schema in a way this
/// build does not know. Nothing was written, so a retry (which a load
/// balancer can send to a current server) is safe.
pub fn fenced() -> Problem {
    Problem::unavailable("schema_fenced")
        .with_detail("This server is older than the database schema. Try again shortly.")
        .with_header(header::RETRY_AFTER, HeaderValue::from_static("1"))
}
