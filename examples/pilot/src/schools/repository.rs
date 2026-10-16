use sqlx::PgConnection;
use trestle_core::identity::ActorId;

use super::{Role, SchoolId};

pub async fn membership_role(
    conn: &mut PgConnection,
    school: SchoolId,
    actor: ActorId,
) -> Result<Option<Role>, sqlx::Error> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM school_memberships WHERE school_id = $1 AND actor_id = $2",
    )
    .bind(school.0)
    .bind(actor.0)
    .fetch_optional(conn)
    .await?;
    role.map(|value| parse_role(&value)).transpose()
}

/// Reads the role and holds a share lock on the membership row until the
/// surrounding transaction ends, so a concurrent revocation serializes with
/// the write that depends on it.
pub async fn lock_membership_role(
    conn: &mut PgConnection,
    school: SchoolId,
    actor: ActorId,
) -> Result<Option<Role>, sqlx::Error> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM school_memberships WHERE school_id = $1 AND actor_id = $2 FOR SHARE",
    )
    .bind(school.0)
    .bind(actor.0)
    .fetch_optional(conn)
    .await?;
    role.map(|value| parse_role(&value)).transpose()
}

fn parse_role(value: &str) -> Result<Role, sqlx::Error> {
    Role::parse(value).ok_or_else(|| sqlx::Error::Decode(format!("unknown role {value:?}").into()))
}
