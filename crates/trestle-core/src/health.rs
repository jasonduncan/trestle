//! System routes. Liveness reports the process; readiness reports the
//! database, required schema capabilities (§15.3) and the schema fence
//! (ADR-017). Neither applies migrations.

use std::sync::Arc;

use axum::Json;
use axum::extract::{FromRef, State};
use axum::routing::{MethodRouter, get};
use serde_json::{Value, json};
use sqlx::PgPool;
use trestle_historical::MigrationRef;

use crate::Problem;

pub const LIVE_PATH: &str = "/healthz";
pub const READY_PATH: &str = "/readyz";

pub(crate) fn live<S>() -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    get(|| async { Json(json!({ "status": "ok" })) })
}

pub(crate) fn ready<S>(required: Arc<[MigrationRef]>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
{
    get(move |State(pool): State<PgPool>| {
        let required = required.clone();
        async move { readiness(&pool, &required).await }
    })
}

async fn readiness(pool: &PgPool, required: &[MigrationRef]) -> Result<Json<Value>, Problem> {
    let unavailable = |_| Problem::unavailable("database_unavailable");
    let mut conn = pool.acquire().await.map_err(unavailable)?;
    let missing = trestle_historical::missing(&mut conn, required)
        .await
        .map_err(unavailable)?;
    if !missing.is_empty() {
        return Err(Problem::unavailable("schema_not_ready")
            .with_detail("Required migrations have not been applied."));
    }
    // A fenced build drops out of rotation (ADR-017).
    crate::fence::check(&mut conn).await?;
    Ok(Json(json!({ "status": "ready" })))
}
