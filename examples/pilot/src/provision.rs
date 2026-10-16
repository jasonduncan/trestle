//! Operator provisioning for the pilot: schools and memberships.
//!
//! Command-line only (`pilot provision ...`), run by an operator with
//! database access; there is no HTTP surface. Every change is audited with
//! the reserved [`OPERATOR`] actor. Removing a membership takes effect on
//! the member's next request: policies read membership on every request
//! and recheck it inside write transactions.

use sqlx::PgPool;
use trestle_core::identity::ActorId;
use uuid::Uuid;

use crate::audit::{self, AuditEvent};
use crate::schools::{Role, SchoolId};

/// Audit actor for operator commands. No identity adapter can produce it.
pub const OPERATOR: ActorId = ActorId(Uuid::nil());

pub async fn create_school(pool: &PgPool, name: &str) -> Result<SchoolId, sqlx::Error> {
    let id = SchoolId(Uuid::now_v7());
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO schools (id, name) VALUES ($1, $2)")
        .bind(id.0)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        AuditEvent {
            actor: OPERATOR,
            tenant: Some(id),
            operation: "provision.school.create",
            operation_version: "1.0.0",
            target_type: "school",
            target_id: id.to_string(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Grants or changes `actor`'s role in `school`.
pub async fn set_membership(
    pool: &PgPool,
    school: SchoolId,
    actor: ActorId,
    role: Role,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO school_memberships (school_id, actor_id, role) VALUES ($1, $2, $3) \
         ON CONFLICT (school_id, actor_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(school.0)
    .bind(actor.0)
    .bind(role.as_str())
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        AuditEvent {
            actor: OPERATOR,
            tenant: Some(school),
            operation: "provision.membership.set",
            operation_version: "1.0.0",
            target_type: "membership",
            target_id: format!("{actor}:{}", role.as_str()),
        },
    )
    .await?;
    tx.commit().await
}

/// Removes `actor` from `school`. Returns whether a membership existed.
pub async fn remove_membership(
    pool: &PgPool,
    school: SchoolId,
    actor: ActorId,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let removed =
        sqlx::query("DELETE FROM school_memberships WHERE school_id = $1 AND actor_id = $2")
            .bind(school.0)
            .bind(actor.0)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
    if removed {
        audit::record(
            &mut tx,
            AuditEvent {
                actor: OPERATOR,
                tenant: Some(school),
                operation: "provision.membership.remove",
                operation_version: "1.0.0",
                target_type: "membership",
                target_id: actor.to_string(),
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(removed)
}
