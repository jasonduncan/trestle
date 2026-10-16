//! Lesson policy. Deny by default; the repository enforces tenant scope.

use sqlx::PgConnection;
use trestle_core::Problem;
use trestle_core::operation::Policy;

use crate::schools::{self, Role, TenantContext, can_write};

/// `lessons.create`: owners and teachers of the selected school.
pub fn may_create(role: Role) -> bool {
    can_write(role)
}

/// `lessons.get`: any member of the selected school.
pub fn may_read(role: Role) -> bool {
    matches!(role, Role::Owner | Role::Teacher | Role::Viewer)
}

pub enum LessonPolicy {
    Create,
    Read,
    Update,
    Delete,
    Publish,
}

impl Policy<TenantContext> for LessonPolicy {
    fn id(&self) -> &'static str {
        match self {
            LessonPolicy::Create => "lessons.create",
            LessonPolicy::Read => "lessons.read",
            LessonPolicy::Update => "lessons.update",
            LessonPolicy::Delete => "lessons.delete",
            LessonPolicy::Publish => "lessons.publish",
        }
    }

    fn admit(&self, ctx: &TenantContext) -> Result<(), Problem> {
        match self {
            LessonPolicy::Read if may_read(ctx.role) => Ok(()),
            // Concealment: a reader without access sees nothing, not a denial.
            LessonPolicy::Read => Err(Problem::not_found()),
            _ if can_write(ctx.role) => Ok(()),
            _ => {
                tracing::info!(role = ctx.role.as_str(), "denied by policy");
                Err(Problem::forbidden("permission_denied"))
            }
        }
    }

    /// Membership may have changed since the request was admitted; writes
    /// recheck under a share lock inside the transaction.
    async fn recheck(&self, conn: &mut PgConnection, ctx: &TenantContext) -> Result<(), Problem> {
        match self {
            LessonPolicy::Read => Ok(()),
            _ => schools::recheck_role(conn, ctx, can_write).await,
        }
    }
}
