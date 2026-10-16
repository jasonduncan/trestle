//! Development fixtures: two schools (tenants) and three actors. Used by
//! `pilot seed-dev` and the integration tests. Never run in production.

use sqlx::PgPool;
use uuid::Uuid;

pub const NORTHSIDE: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
pub const SOUTHSIDE: Uuid = Uuid::from_u128(0x22222222_2222_4222_8222_222222222222);

/// Teacher at Northside.
pub const ALICE: Uuid = Uuid::from_u128(0xaaaaaaaa_aaaa_4aaa_8aaa_aaaaaaaaaaaa);
/// Teacher at Southside.
pub const BOB: Uuid = Uuid::from_u128(0xbbbbbbbb_bbbb_4bbb_8bbb_bbbbbbbbbbbb);
/// Viewer at Northside.
pub const CAROL: Uuid = Uuid::from_u128(0xcccccccc_cccc_4ccc_8ccc_cccccccccccc);
/// Owner at Northside (sees the admin profile).
pub const OLIVIA: Uuid = Uuid::from_u128(0xdddddddd_dddd_4ddd_8ddd_dddddddddddd);

/// Matches `.env.example`.
pub const DEV_TOKENS: &str = "alice-dev-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa,\
bob-dev-token=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb,\
carol-dev-token=cccccccc-cccc-4ccc-8ccc-cccccccccccc,\
olivia-dev-token=dddddddd-dddd-4ddd-8ddd-dddddddddddd";

pub async fn seed(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for (id, name) in [
        (NORTHSIDE, "Northside Elementary"),
        (SOUTHSIDE, "Southside Middle"),
    ] {
        sqlx::query("INSERT INTO schools (id, name) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING")
            .bind(id)
            .bind(name)
            .execute(&mut *tx)
            .await?;
    }
    for (school, actor, role) in [
        (NORTHSIDE, ALICE, "teacher"),
        (SOUTHSIDE, BOB, "teacher"),
        (NORTHSIDE, CAROL, "viewer"),
        (NORTHSIDE, OLIVIA, "owner"),
    ] {
        sqlx::query(
            "INSERT INTO school_memberships (school_id, actor_id, role) VALUES ($1, $2, $3)
             ON CONFLICT (school_id, actor_id) DO NOTHING",
        )
        .bind(school)
        .bind(actor)
        .bind(role)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}
