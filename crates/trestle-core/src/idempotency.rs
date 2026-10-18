//! Idempotency-key receipts (ADR-016, design doc §6.4).
//!
//! A receipt is written in the same transaction as the operation it
//! protects, so "the operation committed" and "its receipt exists" are the
//! same fact:
//!
//! ```text
//! admit -> begin -> recheck -> claim receipt
//!   new key:         run -> check output -> store body -> commit
//!   committed key:   same input -> replay stored body; other input -> 409
//!   key in flight:   the claim waits on the other transaction, then decides
//! ```
//!
//! A failed or rolled-back attempt leaves no receipt, so repeating its key
//! runs again; a repeat after an unknown commit outcome either replays or
//! runs, never both. Replays happen after `admit` and `recheck`: a caller who
//! has lost access is refused, not answered from the receipt.

use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use trestle_historical::schema::{Column, Model};
use uuid::Uuid;

use crate::Problem;
use crate::invocation::Invocation;
use crate::registry::{App, AppBuilder};

pub use trestle_protocol::interaction::{IDEMPOTENCY_KEY_HEADER, IDEMPOTENCY_RETENTION_SECONDS};

/// The receipt table.
pub const TABLE: &str = "trestle_idempotency";
/// Response header on a replayed answer.
pub const REPLAYED_HEADER: &str = "idempotency-replayed";
/// Longest key accepted.
pub const MAX_KEY_CHARS: usize = 255;

/// The receipt table's model (`idempotency.receipt`). Install
/// [`IdempotencyApp`] to add it; `makemigrations` creates the migration.
pub fn model() -> Model {
    Model::new("idempotency.receipt")
        .table(TABLE)
        .field(Column::uuid("id").primary_key())
        .field(Column::text("scope").chars(64, 64))
        .field(Column::text("operation").chars(1, 200))
        .field(Column::uuid("actor_id").nullable())
        .field(Column::text("input_hash").chars(64, 64))
        .field(Column::text("body").nullable())
        .field(Column::timestamptz("created_at").default_now())
        .field(Column::timestamptz("expires_at"))
        .unique("trestle_idempotency_scope_key", &["scope"])
        .index("trestle_idempotency_expires_idx", &["expires_at"])
}

/// Installs the receipt table. Registers no operations or routes.
pub struct IdempotencyApp;

impl<S: 'static> App<S> for IdempotencyApp {
    fn id(&self) -> &str {
        "idempotency"
    }

    fn register(&self, app: &mut AppBuilder<S>) {
        app.model(model());
    }
}

/// A key is 1–255 visible ASCII characters (`!` to `~`).
pub fn valid_key(key: &str) -> bool {
    (1..=MAX_KEY_CHARS).contains(&key.len()) && key.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// 400 `idempotency_unsupported`: a key sent to an operation that does not
/// accept one is refused, so no caller believes it is protected.
pub fn unsupported() -> Problem {
    Problem::malformed(
        "idempotency_unsupported",
        "This operation does not accept an Idempotency-Key.",
    )
}

/// 400 `idempotency_key_invalid`.
pub fn invalid() -> Problem {
    Problem::malformed(
        "idempotency_key_invalid",
        "Idempotency-Key must be 1 to 255 visible ASCII characters.",
    )
}

/// Deletes receipts past their retention, returning how many.
pub async fn purge_expired(conn: &mut PgConnection) -> Result<u64, Problem> {
    let purged = sqlx::query(&format!("DELETE FROM {TABLE} WHERE expires_at < now()"))
        .execute(conn)
        .await?
        .rows_affected();
    Ok(purged)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What one keyed write is claiming.
pub(crate) struct Receipt {
    scope: String,
    operation: String,
    actor: Option<Uuid>,
    input_hash: String,
}

impl Receipt {
    /// Scoped by actor, tenant, operation ID and version, and key; bound to
    /// the canonical (validated, key-sorted) input.
    pub(crate) fn new(
        invocation: &Invocation,
        operation: &str,
        version: &str,
        key: &str,
        input: &Value,
    ) -> Self {
        let actor = invocation.actor().ok().map(|a| a.0);
        let mut scope = Sha256::new();
        for part in [
            actor.map(|a| a.to_string()).unwrap_or_default(),
            invocation
                .tenant()
                .map(|t| t.to_string())
                .unwrap_or_default(),
            operation.to_owned(),
            version.to_owned(),
            key.to_owned(),
        ] {
            // Length-prefixed, so no two different scopes share a byte string.
            scope.update((part.len() as u64).to_be_bytes());
            scope.update(part.as_bytes());
        }
        Self {
            scope: hex(&scope.finalize()),
            operation: operation.to_owned(),
            actor,
            input_hash: hex(&Sha256::digest(input.to_string().as_bytes())),
        }
    }
}

pub(crate) enum Claim {
    /// The key is new here: run the operation, then [`complete`].
    Fresh,
    /// The key already committed with this input: its response body.
    Replay(String),
}

/// Claims `receipt` inside the caller's write transaction. A concurrent
/// claim of the same scope blocks on the unique index until that
/// transaction ends, so duplicates are serialized.
pub(crate) async fn claim(conn: &mut PgConnection, receipt: &Receipt) -> Result<Claim, Problem> {
    sqlx::query(&format!(
        "DELETE FROM {TABLE} WHERE scope = $1 AND expires_at < now()"
    ))
    .bind(&receipt.scope)
    .execute(&mut *conn)
    .await?;
    let inserted = sqlx::query(&format!(
        "INSERT INTO {TABLE} (id, scope, operation, actor_id, input_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))
         ON CONFLICT (scope) DO NOTHING"
    ))
    .bind(Uuid::now_v7())
    .bind(&receipt.scope)
    .bind(&receipt.operation)
    .bind(receipt.actor)
    .bind(&receipt.input_hash)
    .bind(IDEMPOTENCY_RETENTION_SECONDS as f64)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(Claim::Fresh);
    }
    let (input_hash, body): (String, Option<String>) = sqlx::query_as(&format!(
        "SELECT input_hash, body FROM {TABLE} WHERE scope = $1"
    ))
    .bind(&receipt.scope)
    .fetch_one(&mut *conn)
    .await?;
    if input_hash != receipt.input_hash {
        return Err(Problem::conflict("idempotency_key_reused")
            .with_detail("This Idempotency-Key was already used with a different request."));
    }
    // Receipts become visible only when their transaction commits, and that
    // transaction stores the body first; an empty body is a broken row.
    body.map(Claim::Replay).ok_or_else(|| {
        tracing::error!(operation = %receipt.operation, "idempotency receipt has no body");
        Problem::internal()
    })
}

/// Stores the committed response with the receipt, in the same transaction.
pub(crate) async fn complete(
    conn: &mut PgConnection,
    receipt: &Receipt,
    body: &str,
) -> Result<(), Problem> {
    sqlx::query(&format!("UPDATE {TABLE} SET body = $2 WHERE scope = $1"))
        .bind(&receipt.scope)
        .bind(body)
        .execute(conn)
        .await?;
    Ok(())
}

/// The retention, as a `Duration`.
pub fn retention() -> Duration {
    Duration::from_secs(IDEMPOTENCY_RETENTION_SECONDS)
}
