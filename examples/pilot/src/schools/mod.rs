//! Schools are the pilot's tenants.
//!
//! Tenant context comes from verified membership plus an explicit selection
//! (the `Trestle-Tenant` header). The header alone is never authority
//! (§12.1): it only says which of the caller's memberships to use.

pub mod mine;
pub mod repository;

use std::fmt;

use axum::extract::FromRequestParts;
use http::HeaderName;
use http::request::Parts;
use trestle_core::Problem;
use trestle_core::identity::{ActorId, Authenticated};
use trestle_core::operation::{ContextDescriptor, RequestContext};
use trestle_core::registry::{App, AppBuilder, Http};
use trestle_historical::schema::{Column, Model};
use uuid::Uuid;

use crate::AppState;

/// Owns tenants and membership. Its only HTTP surface is the caller's own
/// memberships; provisioning is an operator command (`pilot provision`).
pub struct SchoolsApp;

impl App<AppState> for SchoolsApp {
    fn id(&self) -> &'static str {
        "schools"
    }

    fn register(&self, app: &mut AppBuilder<AppState>) {
        app.schema(pilot_migrations::SCHOOLS_0001)
            .model(school_model())
            .model(membership_model());
        app.operation(mine::MySchools)
            .http(Http::get("/api/v1/me/schools"));
    }
}

pub fn school_model() -> Model {
    Model::new("schools.school")
        .table("schools")
        .field(Column::uuid("id").primary_key())
        .field(Column::text("name").chars(1, 200))
        .field(Column::timestamptz("created_at").default_now())
}

pub fn membership_model() -> Model {
    Model::new("schools.membership")
        .table("school_memberships")
        .field(
            Column::uuid("school_id")
                .primary_key()
                .references("schools.school"),
        )
        .field(Column::uuid("actor_id").primary_key())
        .field(Column::text("role").one_of(&["owner", "teacher", "viewer"]))
        .field(Column::timestamptz("created_at").default_now())
        .index("school_memberships_actor_idx", &["actor_id"])
}

pub const TENANT_HEADER: HeaderName = HeaderName::from_static("trestle-tenant");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SchoolId(pub Uuid);

impl fmt::Display for SchoolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Teacher,
    Viewer,
}

impl Role {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Role::Owner),
            "teacher" => Some(Role::Teacher),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Teacher => "teacher",
            Role::Viewer => "viewer",
        }
    }
}

/// Trusted per-request context: who is calling, for which school, with
/// which role at the time the request was admitted. Writes recheck the role
/// inside their transaction.
#[derive(Debug, Clone, Copy)]
pub struct TenantContext {
    pub actor: ActorId,
    pub school: SchoolId,
    pub role: Role,
}

/// Owners and teachers may change school content.
pub fn can_write(role: Role) -> bool {
    matches!(role, Role::Owner | Role::Teacher)
}

/// Recheck, inside the write transaction and under a share lock, that the
/// caller still holds a role `allowed` accepts (§6.2).
pub async fn recheck_role(
    conn: &mut sqlx::PgConnection,
    ctx: &TenantContext,
    allowed: fn(Role) -> bool,
) -> Result<(), Problem> {
    let role = repository::lock_membership_role(conn, ctx.school, ctx.actor).await?;
    if role.is_some_and(allowed) {
        Ok(())
    } else {
        tracing::info!("denied on transactional recheck");
        Err(Problem::forbidden("permission_denied"))
    }
}

/// The admin publication profile: owners of the selected school. It only
/// decides what admin metadata a caller sees; operations still authorize.
pub struct AdminAccess;

impl trestle_core::operation::Policy<TenantContext> for AdminAccess {
    fn id(&self) -> &'static str {
        "schools.admin"
    }

    fn admit(&self, ctx: &TenantContext) -> Result<(), Problem> {
        if ctx.role == Role::Owner {
            Ok(())
        } else {
            Err(Problem::forbidden("admin_forbidden"))
        }
    }

    async fn recheck(
        &self,
        conn: &mut sqlx::PgConnection,
        ctx: &TenantContext,
    ) -> Result<(), Problem> {
        recheck_role(conn, ctx, |role| role == Role::Owner).await
    }
}

impl RequestContext for TenantContext {
    fn describe() -> ContextDescriptor {
        ContextDescriptor::authenticated().header(
            "Trestle-Tenant",
            "School to act in. Must be one of the caller's memberships; grants nothing by itself.",
            Some("uuid"),
        )
    }
}

impl FromRequestParts<AppState> for TenantContext {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Authenticated(actor) = Authenticated::from_request_parts(parts, state).await?;
        let selected = parts.headers.get(TENANT_HEADER).ok_or_else(|| {
            Problem::malformed(
                "tenant_required",
                "Select a school with the Trestle-Tenant header.",
            )
        })?;
        let school = selected
            .to_str()
            .ok()
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
            .map(SchoolId)
            .ok_or_else(|| {
                Problem::malformed("tenant_invalid", "Trestle-Tenant must be a school ID.")
            })?;
        let mut conn = state.pool.acquire().await?;
        // Unknown schools and schools without membership look the same.
        let role = repository::membership_role(&mut conn, school, actor)
            .await?
            .ok_or_else(|| Problem::forbidden("tenant_not_permitted"))?;
        Ok(TenantContext {
            actor,
            school,
            role,
        })
    }
}
