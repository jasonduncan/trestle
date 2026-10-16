//! Lesson operations: the one place their behavior lives. The executor in
//! `trestle_core::operation` runs them for every caller (managed routes,
//! the admin shell, hand-written handlers, tests), after validation and
//! authorization, inside the transaction it owns (I04).

use serde_json::{Value, json};
use sqlx::PgConnection;
use trestle_core::db::unique_violation;
use trestle_core::operation::{Mode, Operation, OperationInfo};
use trestle_core::pagination::{Page, decode_cursor, page_from, page_size};
use trestle_core::protocol::Audience;
use trestle_core::{FieldError, Problem};
use uuid::Uuid;

use super::contracts::{
    CreateLessonInput, DeleteLessonInput, GetLessonInput, LessonStatus, LessonView,
    ListLessonsInput, PatchLessonInput, PublishLessonInput,
};
use super::policy::LessonPolicy;
use super::repository::{self, LessonChanges, ListFilters, NewLesson, TITLE_UNIQUE};
use crate::audit::{self, AuditEvent};
use crate::schools::TenantContext;
use crate::units;

const AUDIENCES: &[Audience] = &[Audience::Product, Audience::Admin];

fn title_taken(err: sqlx::Error) -> Problem {
    match unique_violation(&err) {
        Some(TITLE_UNIQUE) => Problem::validation(vec![FieldError::new(
            "/title",
            "already_exists",
            "A lesson with this title already exists in this school.",
        )]),
        _ => Problem::from(err),
    }
}

/// A chosen unit must belong to the caller's school. A unit elsewhere looks
/// exactly like one that does not exist (§12.3: hidden relation targets).
async fn check_unit(
    conn: &mut PgConnection,
    ctx: &TenantContext,
    unit: Uuid,
) -> Result<(), Problem> {
    if units::exists_in_school(conn, ctx.school, unit).await? {
        Ok(())
    } else {
        Err(Problem::validation(vec![FieldError::new(
            "/unit_id",
            "not_found",
            "Choose a unit from this school.",
        )]))
    }
}

async fn record(
    conn: &mut PgConnection,
    ctx: &TenantContext,
    info: OperationInfo,
    lesson: Uuid,
) -> Result<(), Problem> {
    audit::record(
        conn,
        AuditEvent {
            actor: ctx.actor,
            tenant: Some(ctx.school),
            operation: info.id,
            operation_version: info.version,
            target_type: "lesson",
            target_id: lesson.to_string(),
        },
    )
    .await?;
    Ok(())
}

fn version(expected: &str) -> Result<i64, Problem> {
    // Digits by contract; too large to be any real version means stale.
    expected.parse().map_err(|_| Problem::precondition_failed())
}

/// Creates a draft lesson in the caller's school (§6.2 command sequence).
pub struct CreateLesson;

impl Operation for CreateLesson {
    type Context = TenantContext;
    type Input = CreateLessonInput;
    type Output = LessonView;
    type Policy = LessonPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "lessons.create",
        version: "1.0.0",
        mode: Mode::Write,
        audiences: AUDIENCES,
        summary: "Create a draft lesson in the selected school.",
    };

    fn policy(&self) -> &LessonPolicy {
        &LessonPolicy::Create
    }

    #[tracing::instrument(name = "lessons.create", skip_all, fields(school = %ctx.school, actor = %ctx.actor))]
    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: CreateLessonInput,
    ) -> Result<LessonView, Problem> {
        if let Some(unit) = input.unit_id {
            check_unit(conn, ctx, unit).await?;
        }
        let row = repository::insert(
            conn,
            NewLesson {
                id: Uuid::now_v7(),
                school: ctx.school,
                title: &input.title,
                grade_level: input.grade_level,
                subtitle: input.subtitle.as_deref(),
                unit_id: input.unit_id,
                duration_minutes: input.duration_minutes,
                created_by: ctx.actor,
            },
        )
        .await
        .map_err(title_taken)?;
        record(conn, ctx, Self::INFO, row.id).await?;
        tracing::info!(lesson = %row.id, "lesson created");
        Ok(row.into_view()?)
    }
}

/// Reads one lesson in the caller's school. Lessons in other schools are
/// indistinguishable from missing ones.
pub struct GetLesson;

impl Operation for GetLesson {
    type Context = TenantContext;
    type Input = GetLessonInput;
    type Output = LessonView;
    type Policy = LessonPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "lessons.get",
        version: "1.0.0",
        mode: Mode::Read,
        audiences: AUDIENCES,
        summary: "Read one lesson in the selected school.",
    };

    fn policy(&self) -> &LessonPolicy {
        &LessonPolicy::Read
    }

    #[tracing::instrument(name = "lessons.get", skip_all, fields(school = %ctx.school, actor = %ctx.actor))]
    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: GetLessonInput,
    ) -> Result<LessonView, Problem> {
        let row = repository::get(conn, ctx.school, input.id)
            .await?
            .ok_or_else(Problem::not_found)?;
        Ok(row.into_view()?)
    }
}

/// Lists lessons in the caller's school by title, with allowlisted filters.
pub struct ListLessons;

impl Operation for ListLessons {
    type Context = TenantContext;
    type Input = ListLessonsInput;
    type Output = Page<LessonView>;
    type Policy = LessonPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "lessons.list",
        version: "1.0.0",
        mode: Mode::Read,
        audiences: AUDIENCES,
        summary: "List lessons in the selected school by title.",
    };

    fn policy(&self) -> &LessonPolicy {
        &LessonPolicy::Read
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: ListLessonsInput,
    ) -> Result<Page<LessonView>, Problem> {
        let limit = page_size(input.limit, 20);
        let filters = ListFilters {
            grade_level: input.grade_level,
            status: input.status,
            unit_id: input.unit_id,
        };
        // The cursor is only valid for exactly this query.
        let shape = json!({
            "op": Self::INFO.id,
            "limit": limit,
            "grade_level": filters.grade_level,
            "status": filters.status.map(LessonStatus::as_str),
            "unit_id": filters.unit_id,
        });
        let after = match &input.cursor {
            Some(cursor) => Some(position(&decode_cursor(&shape, cursor)?)?),
            None => None,
        };
        let rows = repository::list(conn, ctx.school, &filters, after, limit + 1).await?;
        page_from(
            rows,
            limit,
            &shape,
            |r| json!([r.title, r.id]),
            |r| r.into_view().map_err(Problem::from),
        )
    }
}

fn position(value: &Value) -> Result<(String, Uuid), Problem> {
    let invalid = || Problem::malformed("invalid_cursor", "The cursor is not valid.");
    let title = value[0].as_str().ok_or_else(invalid)?.to_owned();
    let id = value[1]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(invalid)?;
    Ok((title, id))
}

/// Partial update with `If-Match` (§5.3, §6.4).
pub struct PatchLesson;

impl Operation for PatchLesson {
    type Context = TenantContext;
    type Input = PatchLessonInput;
    type Output = LessonView;
    type Policy = LessonPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "lessons.update",
        version: "1.0.0",
        mode: Mode::Write,
        audiences: AUDIENCES,
        summary: "Change a lesson's title, grade, subtitle, unit or duration.",
    };

    fn policy(&self) -> &LessonPolicy {
        &LessonPolicy::Update
    }

    #[tracing::instrument(name = "lessons.update", skip_all, fields(school = %ctx.school, actor = %ctx.actor))]
    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: PatchLessonInput,
    ) -> Result<LessonView, Problem> {
        let expected = version(&input.expected_version)?;
        let changes = LessonChanges {
            title: input.title.change().flatten(),
            grade_level: input.grade_level.change().flatten(),
            subtitle: input.subtitle.change(),
            unit_id: input.unit_id.change(),
            duration_minutes: input.duration_minutes.change(),
        };
        if let Some(Some(unit)) = changes.unit_id {
            check_unit(conn, ctx, unit).await?;
        }
        if changes.is_empty() {
            // Nothing to change, but the precondition still applies.
            let row = repository::get(conn, ctx.school, input.id)
                .await?
                .ok_or_else(Problem::not_found)?;
            return if row.version == expected {
                Ok(row.into_view()?)
            } else {
                Err(Problem::precondition_failed())
            };
        }
        match repository::update(conn, ctx.school, input.id, expected, changes)
            .await
            .map_err(title_taken)?
        {
            Some(row) => {
                record(conn, ctx, Self::INFO, row.id).await?;
                Ok(row.into_view()?)
            }
            None => {
                Err(missing_or_stale(conn, ctx, input.id, Problem::precondition_failed()).await)
            }
        }
    }
}

/// When a guarded write matched nothing: 404 if the lesson is not in this
/// school, otherwise `stale`.
async fn missing_or_stale(
    conn: &mut PgConnection,
    ctx: &TenantContext,
    id: Uuid,
    stale: Problem,
) -> Problem {
    match repository::get(conn, ctx.school, id).await {
        Ok(Some(_)) => stale,
        Ok(None) => Problem::not_found(),
        Err(err) => Problem::from(err),
    }
}

/// Deletes a lesson with `If-Match`; 204. Typed callers get the deleted
/// representation, which is also what `If-Match` is checked against.
pub struct DeleteLesson;

impl Operation for DeleteLesson {
    type Context = TenantContext;
    type Input = DeleteLessonInput;
    type Output = LessonView;
    type Policy = LessonPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "lessons.delete",
        version: "1.0.0",
        mode: Mode::Write,
        audiences: AUDIENCES,
        summary: "Delete a lesson.",
    };

    fn policy(&self) -> &LessonPolicy {
        &LessonPolicy::Delete
    }

    #[tracing::instrument(name = "lessons.delete", skip_all, fields(school = %ctx.school, actor = %ctx.actor))]
    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: DeleteLessonInput,
    ) -> Result<LessonView, Problem> {
        let expected = version(&input.expected_version)?;
        let Some(row) = repository::delete(conn, ctx.school, input.id, expected).await? else {
            return Err(
                missing_or_stale(conn, ctx, input.id, Problem::precondition_failed()).await,
            );
        };
        record(conn, ctx, Self::INFO, row.id).await?;
        Ok(row.into_view()?)
    }
}

/// Domain action: draft → published. Stale versions and wrong states are
/// operation-level conflicts, not field errors (§7.2).
pub struct PublishLesson;

impl Operation for PublishLesson {
    type Context = TenantContext;
    type Input = PublishLessonInput;
    type Output = LessonView;
    type Policy = LessonPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "lessons.publish",
        version: "1.0.0",
        mode: Mode::Write,
        audiences: AUDIENCES,
        summary: "Publish a draft lesson.",
    };

    fn policy(&self) -> &LessonPolicy {
        &LessonPolicy::Publish
    }

    #[tracing::instrument(name = "lessons.publish", skip_all, fields(school = %ctx.school, actor = %ctx.actor))]
    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: PublishLessonInput,
    ) -> Result<LessonView, Problem> {
        let stale = || {
            Problem::conflict("stale_version")
                .with_detail("The lesson changed since you loaded it. Reload it and try again.")
        };
        let expected = input.expected_version.parse::<i64>().map_err(|_| stale())?;
        let current = repository::get(conn, ctx.school, input.id)
            .await?
            .ok_or_else(Problem::not_found)?;
        if current.version != expected {
            return Err(stale());
        }
        if current.status != LessonStatus::Draft.as_str() {
            return Err(Problem::conflict("invalid_transition")
                .with_detail("Only draft lessons can be published."));
        }
        let row = repository::publish(conn, ctx.school, input.id, expected)
            .await?
            .ok_or_else(stale)?;
        record(conn, ctx, Self::INFO, row.id).await?;
        tracing::info!(lesson = %row.id, "lesson published");
        Ok(row.into_view()?)
    }
}
