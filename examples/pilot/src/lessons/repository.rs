//! Lesson persistence. Every query is scoped by school; every function takes
//! an explicit connection and never reaches for a pool (§6.3, §13). Reads
//! use the typed query surface, which cannot express an unscoped query.
//! Writes are explicit SQL whose `WHERE` carries the scope *and* the
//! expected version, so a stale or foreign write matches nothing (§6.2).

use sqlx::{FromRow, PgConnection, Postgres, QueryBuilder};
use trestle_core::identity::ActorId;
use trestle_core::query::{self, Col, Direction, Table};
use uuid::Uuid;

use super::contracts::{LessonStatus, LessonView};
use crate::schools::SchoolId;

/// Name of the per-school unique title constraint (lessons.0001_initial).
pub const TITLE_UNIQUE: &str = "lessons_school_title_key";

const RETURNING: &str = " RETURNING id, school_id, title, grade_level, status, version, subtitle, unit_id, duration_minutes";

/// Typed handles for `lessons.lesson` (checked against the model in tests).
pub struct Lessons;

impl Table for Lessons {
    const MODEL: &'static str = "lessons.lesson";
    const TABLE: &'static str = "lessons";
    const COLUMNS: &'static [&'static str] = &[
        "id",
        "school_id",
        "title",
        "grade_level",
        "status",
        "version",
        "subtitle",
        "unit_id",
        "duration_minutes",
    ];
    const PRIMARY_KEY: &'static str = "id";
}

pub const ID: Col<Lessons, Uuid> = Col::new("id");
pub const SCHOOL_ID: Col<Lessons, Uuid> = Col::new("school_id");
pub const TITLE: Col<Lessons, String> = Col::new("title");
pub const GRADE_LEVEL: Col<Lessons, i16> = Col::new("grade_level");
pub const STATUS: Col<Lessons, String> = Col::new("status");
pub const SUBTITLE: Col<Lessons, Option<String>> = Col::new("subtitle");
pub const UNIT_ID: Col<Lessons, Uuid> = Col::new("unit_id");

#[derive(Debug, Clone, FromRow)]
pub struct LessonRow {
    pub id: Uuid,
    pub school_id: Uuid,
    pub title: String,
    pub grade_level: i16,
    pub status: String,
    pub version: i64,
    pub subtitle: Option<String>,
    pub unit_id: Option<Uuid>,
    pub duration_minutes: Option<i16>,
}

impl LessonRow {
    /// Explicit output projection: only the fields `lessons.view` exposes.
    pub fn into_view(self) -> Result<LessonView, sqlx::Error> {
        let status = LessonStatus::parse(&self.status).ok_or_else(|| {
            sqlx::Error::Decode(format!("unknown lesson status {:?}", self.status).into())
        })?;
        Ok(LessonView {
            id: self.id,
            title: self.title,
            grade_level: self.grade_level,
            status,
            version: self.version.to_string(),
            subtitle: self.subtitle,
            unit_id: self.unit_id,
            duration_minutes: self.duration_minutes,
        })
    }
}

pub struct NewLesson<'a> {
    pub id: Uuid,
    pub school: SchoolId,
    pub title: &'a str,
    pub grade_level: i16,
    pub subtitle: Option<&'a str>,
    pub unit_id: Option<Uuid>,
    pub duration_minutes: Option<i16>,
    pub created_by: ActorId,
}

pub async fn insert(conn: &mut PgConnection, new: NewLesson<'_>) -> Result<LessonRow, sqlx::Error> {
    sqlx::query_as(&format!(
        "INSERT INTO lessons (id, school_id, title, grade_level, subtitle, unit_id, duration_minutes, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8){RETURNING}"
    ))
    .bind(new.id)
    .bind(new.school.0)
    .bind(new.title)
    .bind(new.grade_level)
    .bind(new.subtitle)
    .bind(new.unit_id)
    .bind(new.duration_minutes)
    .bind(new.created_by.0)
    .fetch_one(conn)
    .await
}

pub async fn get(
    conn: &mut PgConnection,
    school: SchoolId,
    id: Uuid,
) -> Result<Option<LessonRow>, sqlx::Error> {
    query::select(SCHOOL_ID, school.0)
        .eq(ID, id)
        .fetch_optional(conn)
        .await
}

/// Allowlisted list filters.
#[derive(Debug, Clone, Default)]
pub struct ListFilters {
    pub grade_level: Option<i16>,
    pub status: Option<LessonStatus>,
    pub unit_id: Option<Uuid>,
}

/// Lessons in `school` ordered by title then ID, after `after` (keyset).
pub async fn list(
    conn: &mut PgConnection,
    school: SchoolId,
    filters: &ListFilters,
    after: Option<(String, Uuid)>,
    limit: i64,
) -> Result<Vec<LessonRow>, sqlx::Error> {
    let mut select = query::select(SCHOOL_ID, school.0);
    if let Some(grade) = filters.grade_level {
        select = select.eq(GRADE_LEVEL, grade);
    }
    if let Some(status) = filters.status {
        select = select.eq(STATUS, status.as_str().to_owned());
    }
    if let Some(unit) = filters.unit_id {
        select = select.eq(UNIT_ID, unit);
    }
    if let Some((title, id)) = after {
        select = select.after(TITLE, title, ID, id, Direction::Asc);
    }
    select
        .order_by(TITLE, Direction::Asc)
        .limit(limit)
        .fetch_all(conn)
        .await
}

/// Field changes for [`update`]: `None` leaves a field alone, `Some(None)`
/// clears a nullable one.
#[derive(Debug, Clone, Default)]
pub struct LessonChanges {
    pub title: Option<String>,
    pub grade_level: Option<i16>,
    pub subtitle: Option<Option<String>>,
    pub unit_id: Option<Option<Uuid>>,
    pub duration_minutes: Option<Option<i16>>,
}

impl LessonChanges {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.grade_level.is_none()
            && self.subtitle.is_none()
            && self.unit_id.is_none()
            && self.duration_minutes.is_none()
    }
}

fn guarded(builder: &mut QueryBuilder<'_, Postgres>, school: SchoolId, id: Uuid, expected: i64) {
    builder
        .push(" WHERE school_id = ")
        .push_bind(school.0)
        .push(" AND id = ")
        .push_bind(id)
        .push(" AND version = ")
        .push_bind(expected);
}

/// Applies `changes` if the row is still at `expected`; `None` otherwise.
pub async fn update(
    conn: &mut PgConnection,
    school: SchoolId,
    id: Uuid,
    expected: i64,
    changes: LessonChanges,
) -> Result<Option<LessonRow>, sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new(
        "UPDATE lessons SET version = version + 1, updated_at = now()",
    );
    if let Some(title) = changes.title {
        builder.push(", title = ").push_bind(title);
    }
    if let Some(grade) = changes.grade_level {
        builder.push(", grade_level = ").push_bind(grade);
    }
    if let Some(subtitle) = changes.subtitle {
        builder.push(", subtitle = ").push_bind(subtitle);
    }
    if let Some(unit) = changes.unit_id {
        builder.push(", unit_id = ").push_bind(unit);
    }
    if let Some(minutes) = changes.duration_minutes {
        builder.push(", duration_minutes = ").push_bind(minutes);
    }
    guarded(&mut builder, school, id, expected);
    builder.push(RETURNING);
    builder.build_query_as().fetch_optional(conn).await
}

/// Draft → published if the row is still a draft at `expected`.
pub async fn publish(
    conn: &mut PgConnection,
    school: SchoolId,
    id: Uuid,
    expected: i64,
) -> Result<Option<LessonRow>, sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new(
        "UPDATE lessons SET status = 'published', version = version + 1, updated_at = now()",
    );
    guarded(&mut builder, school, id, expected);
    builder.push(" AND status = 'draft'");
    builder.push(RETURNING);
    builder.build_query_as().fetch_optional(conn).await
}

/// Deletes the row if it is still at `expected`, returning what was deleted.
pub async fn delete(
    conn: &mut PgConnection,
    school: SchoolId,
    id: Uuid,
    expected: i64,
) -> Result<Option<LessonRow>, sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new("DELETE FROM lessons");
    guarded(&mut builder, school, id, expected);
    builder.push(RETURNING);
    builder.build_query_as().fetch_optional(conn).await
}
