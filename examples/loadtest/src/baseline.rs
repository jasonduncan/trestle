//! The hand-written Axum baseline (design doc §19.3).
//!
//! It serves the three reference routes with the same work the pilot does
//! through Trestle — the same SQL statements in the same order, the same
//! pool acquisitions, token digests, tenant membership check, in-transaction
//! role recheck under a share lock, input validation, audit row, cursor
//! signing, ETag and Location — and nothing else: no registry, no contract
//! IR, no executor, no perimeter (admission, rate limits, metrics, security
//! headers) and no tracing. The difference is the framework's overhead.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// Same projection the typed query surface renders for `lessons.lesson`.
const COLUMNS: &str =
    "id, school_id, title, grade_level, status, version, subtitle, unit_id, duration_minutes";

#[derive(Clone)]
pub struct Baseline {
    pool: PgPool,
    tokens: Arc<HashMap<[u8; 32], Uuid>>,
    cursor_key: Arc<Vec<u8>>,
}

impl Baseline {
    pub fn new(pool: PgPool, dev_tokens: &str) -> Self {
        let tokens = dev_tokens
            .split(',')
            .filter_map(|entry| entry.split_once('='))
            .map(|(token, actor)| {
                (
                    Sha256::digest(token.trim().as_bytes()).into(),
                    Uuid::parse_str(actor.trim()).expect("actor UUID"),
                )
            })
            .collect();
        let mut key = Vec::new();
        key.extend_from_slice(Uuid::new_v4().as_bytes());
        key.extend_from_slice(Uuid::new_v4().as_bytes());
        Self {
            pool,
            tokens: Arc::new(tokens),
            cursor_key: Arc::new(key),
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/api/v1/lessons", get(list).post(create))
            .route("/api/v1/lessons/{id}", get(retrieve))
            .with_state(self)
    }
}

fn problem(status: StatusCode, code: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/problem+json")],
        json!({ "status": status.as_u16(), "code": code }).to_string(),
    )
        .into_response()
}

struct Caller {
    actor: Uuid,
    school: Uuid,
    role: String,
}

// Returning the ready response keeps the handlers as plain as a hand-written
// service would be; the size of `Response` does not matter here.
#[allow(clippy::result_large_err)]
async fn caller(state: &Baseline, headers: &HeaderMap) -> Result<Caller, Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| problem(StatusCode::UNAUTHORIZED, "credentials_missing"))?;
    let digest: [u8; 32] = Sha256::digest(token.trim().as_bytes()).into();
    let actor = *state
        .tokens
        .get(&digest)
        .ok_or_else(|| problem(StatusCode::UNAUTHORIZED, "credentials_invalid"))?;
    let school = headers
        .get("trestle-tenant")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v.trim()).ok())
        .ok_or_else(|| problem(StatusCode::BAD_REQUEST, "tenant_required"))?;
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|_| problem(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable"))?;
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM school_memberships WHERE school_id = $1 AND actor_id = $2",
    )
    .bind(school)
    .bind(actor)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|_| problem(StatusCode::INTERNAL_SERVER_ERROR, "internal"))?;
    let role = role.ok_or_else(|| problem(StatusCode::FORBIDDEN, "tenant_not_permitted"))?;
    Ok(Caller {
        actor,
        school,
        role,
    })
}

#[derive(FromRow)]
struct Row {
    id: Uuid,
    #[allow(dead_code)]
    school_id: Uuid,
    title: String,
    grade_level: i16,
    status: String,
    version: i64,
    subtitle: Option<String>,
    unit_id: Option<Uuid>,
    duration_minutes: Option<i16>,
}

impl Row {
    fn view(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "grade_level": self.grade_level,
            "status": self.status,
            "version": self.version.to_string(),
            "subtitle": self.subtitle,
            "unit_id": self.unit_id,
            "duration_minutes": self.duration_minutes,
        })
    }

    fn etag(&self) -> HeaderValue {
        HeaderValue::from_str(&format!("\"lessons.view:{}\"", self.version)).expect("ascii")
    }
}

fn json_response(
    status: StatusCode,
    body: &Value,
    extra: &[(header::HeaderName, HeaderValue)],
) -> Response {
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response();
    for (name, value) in extra {
        response.headers_mut().insert(name.clone(), value.clone());
    }
    response
}

async fn retrieve(
    State(state): State<Baseline>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let caller = match caller(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return problem(StatusCode::BAD_REQUEST, "malformed");
    };
    let Ok(mut conn) = state.pool.acquire().await else {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    let row: Option<Row> = match sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM lessons WHERE school_id = $1 AND id = $2 ORDER BY id ASC LIMIT 100"
    ))
    .bind(caller.school)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    {
        Ok(row) => row,
        Err(_) => return problem(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    match row {
        Some(row) => json_response(StatusCode::OK, &row.view(), &[(header::ETAG, row.etag())]),
        None => problem(StatusCode::NOT_FOUND, "not_found"),
    }
}

fn cursor(state: &Baseline, shape: &Value, position: &Value) -> String {
    let digest: String = Sha256::digest(serde_json::to_vec(shape).unwrap_or_default())
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect();
    let payload = serde_json::to_vec(&json!({ "q": digest, "p": position })).unwrap_or_default();
    let mut mac = Hmac::<Sha256>::new_from_slice(&state.cursor_key).expect("any key");
    mac.update(&payload);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

/// First page only (the reference workload does not follow cursors).
async fn list(State(state): State<Baseline>, headers: HeaderMap) -> Response {
    let caller = match caller(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let limit: i64 = 20;
    let Ok(mut conn) = state.pool.acquire().await else {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    let mut rows: Vec<Row> = match sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM lessons WHERE school_id = $1 ORDER BY title ASC, id ASC LIMIT {}",
        limit + 1
    ))
    .bind(caller.school)
    .fetch_all(&mut *conn)
    .await
    {
        Ok(rows) => rows,
        Err(_) => return problem(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let shape = json!({ "op": "lessons.list", "limit": limit, "grade_level": null, "status": null, "unit_id": null });
    let next = if more {
        rows.last()
            .map(|r| cursor(&state, &shape, &json!([r.title, r.id])))
    } else {
        None
    };
    let items: Vec<Value> = rows.iter().map(Row::view).collect();
    json_response(
        StatusCode::OK,
        &json!({ "items": items, "next_cursor": next }),
        &[],
    )
}

fn chars_between(value: &Value, min: usize, max: usize) -> Option<String> {
    let text = value.as_str()?;
    let n = text.chars().count();
    (min..=max).contains(&n).then(|| text.to_owned())
}

fn integer_between(value: &Value, min: i64, max: i64) -> Option<i64> {
    let n = value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|f| f.fract() == 0.0)
            .map(|f| f as i64)
    })?;
    (min..=max).contains(&n).then_some(n)
}

/// Title, grade, subtitle, unit and duration after validation.
type CreateInput = (String, i16, Option<String>, Option<Uuid>, Option<i16>);

/// The same checks `lessons.create.input` 1.3.0 makes, written by hand.
fn validate(body: &Map<String, Value>) -> Result<CreateInput, Vec<&'static str>> {
    let mut errors = Vec::new();
    for key in body.keys() {
        if !matches!(
            key.as_str(),
            "title" | "grade_level" | "subtitle" | "unit_id" | "duration_minutes"
        ) {
            errors.push("unknown_field");
        }
    }
    let title = body.get("title").and_then(|v| chars_between(v, 1, 200));
    if title.is_none() {
        errors.push("title");
    }
    let grade = body
        .get("grade_level")
        .and_then(|v| integer_between(v, 1, 12));
    if grade.is_none() {
        errors.push("grade_level");
    }
    let subtitle = match body.get("subtitle") {
        None => None,
        Some(v) => match chars_between(v, 1, 200) {
            Some(s) => Some(s),
            None => {
                errors.push("subtitle");
                None
            }
        },
    };
    let unit = match body.get("unit_id") {
        None => None,
        Some(v) => match v.as_str().and_then(|s| Uuid::parse_str(s).ok()) {
            Some(u) => Some(u),
            None => {
                errors.push("unit_id");
                None
            }
        },
    };
    let duration = match body.get("duration_minutes") {
        None => None,
        Some(v) => match integer_between(v, 1, 600) {
            Some(d) => Some(d as i16),
            None => {
                errors.push("duration_minutes");
                None
            }
        },
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok((
        title.unwrap_or_default(),
        grade.unwrap_or_default() as i16,
        subtitle,
        unit,
        duration,
    ))
}

async fn create(State(state): State<Baseline>, headers: HeaderMap, body: Bytes) -> Response {
    let request_id = format!("req-{}", Uuid::now_v7().simple());
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some("application/json")
    {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
    }
    let caller = match caller(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&body) else {
        return problem(StatusCode::BAD_REQUEST, "malformed");
    };
    let (title, grade, subtitle, unit, duration) = match validate(&body) {
        Ok(input) => input,
        Err(_) => return problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed"),
    };
    if unit.is_some() {
        return problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed"); // not exercised
    }
    if !matches!(caller.role.as_str(), "owner" | "teacher") {
        return problem(StatusCode::FORBIDDEN, "permission_denied");
    }
    let result: Result<Option<Row>, sqlx::Error> = async {
        let mut tx = state.pool.begin().await?;
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM school_memberships WHERE school_id = $1 AND actor_id = $2 FOR SHARE",
        )
        .bind(caller.school)
        .bind(caller.actor)
        .fetch_optional(&mut *tx)
        .await?;
        if !role.is_some_and(|r| r == "owner" || r == "teacher") {
            return Ok(None);
        }
        let row: Row = sqlx::query_as(&format!(
            "INSERT INTO lessons (id, school_id, title, grade_level, subtitle, unit_id, duration_minutes, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {COLUMNS}"
        ))
        .bind(Uuid::now_v7())
        .bind(caller.school)
        .bind(&title)
        .bind(grade)
        .bind(&subtitle)
        .bind(unit)
        .bind(duration)
        .bind(caller.actor)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO audit_events
                 (id, actor_id, tenant_id, operation, operation_version, target_type, target_id, request_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::now_v7())
        .bind(caller.actor)
        .bind(caller.school)
        .bind("lessons.create")
        .bind("1.3.0")
        .bind("lesson")
        .bind(row.id.to_string())
        .bind(&request_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(row))
    }
    .await;
    match result {
        Ok(Some(row)) => json_response(
            StatusCode::CREATED,
            &row.view(),
            &[
                (
                    header::LOCATION,
                    HeaderValue::from_str(&format!("/api/v1/lessons/{}", row.id)).expect("ascii"),
                ),
                (header::ETAG, row.etag()),
                (
                    header::HeaderName::from_static("x-request-id"),
                    HeaderValue::from_str(&request_id).expect("ascii"),
                ),
            ],
        ),
        Ok(None) => problem(StatusCode::FORBIDDEN, "permission_denied"),
        Err(err)
            if err
                .as_database_error()
                .is_some_and(|d| d.is_unique_violation()) =>
        {
            problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed")
        }
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}
