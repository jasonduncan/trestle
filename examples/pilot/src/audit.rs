//! Audit facts for successful state changes, written inside the operation's
//! transaction so they commit or roll back with the change (§10.3).

use sqlx::PgConnection;
use trestle_core::identity::ActorId;
use trestle_core::registry::{App, AppBuilder};
use trestle_historical::schema::{Column, Model};
use uuid::Uuid;

use crate::AppState;
use crate::schools::SchoolId;

/// Owns the audit log. Other apps write to it through [`record`].
pub struct AuditApp;

impl App<AppState> for AuditApp {
    fn id(&self) -> &'static str {
        "audit"
    }

    fn register(&self, app: &mut AppBuilder<AppState>) {
        app.schema(pilot_migrations::AUDIT_0001)
            .model(event_model());
    }
}

pub fn event_model() -> Model {
    Model::new("audit.event")
        .table("audit_events")
        .field(Column::uuid("id").primary_key())
        .field(Column::timestamptz("occurred_at").default_now())
        .field(Column::uuid("actor_id"))
        .field(Column::uuid("tenant_id").nullable())
        .field(Column::text("operation"))
        .field(Column::text("operation_version"))
        .field(Column::text("target_type"))
        .field(Column::text("target_id"))
        .field(Column::text("request_id").nullable())
        .index("audit_events_tenant_idx", &["tenant_id", "occurred_at"])
}

pub struct AuditEvent<'a> {
    pub actor: ActorId,
    pub tenant: Option<SchoolId>,
    pub operation: &'a str,
    pub operation_version: &'a str,
    pub target_type: &'a str,
    pub target_id: String,
}

pub async fn record(conn: &mut PgConnection, event: AuditEvent<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_id, tenant_id, operation, operation_version, target_type, target_id, request_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7())
    .bind(event.actor.0)
    .bind(event.tenant.map(|school| school.0))
    .bind(event.operation)
    .bind(event.operation_version)
    .bind(event.target_type)
    .bind(event.target_id)
    .bind(trestle_core::request_id::current())
    .execute(conn)
    .await?;
    Ok(())
}
