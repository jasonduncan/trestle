//! The sign-in audit: one row per authentication attempt or change.
//!
//! An optional app: install [`AuthAuditApp`] with its migration and wrap the
//! router with [`layer`]. The layer observes the identity routes (password
//! sign-in, set-from-link, change, logout, OIDC callback) and records the
//! outcome, the email the attempt named (sign-in only), the session's actor
//! when known, and the client address. It never records passwords, tokens
//! or request bodies beyond the email. Projects record their own events
//! (keys issued, accounts invited) with [`record`].

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;
use trestle_core::identity::ActorId;
use trestle_core::input;
use trestle_core::registry::{App, AppBuilder};
use trestle_historical::MigrationRef;
use trestle_historical::schema::{Column, Model};
use uuid::Uuid;

use crate::{password, routes};

pub struct AuthAuditApp {
    schema: &'static [MigrationRef],
}

impl AuthAuditApp {
    pub const fn new(schema: &'static [MigrationRef]) -> Self {
        Self { schema }
    }
}

impl<S: Clone + Send + Sync + 'static> App<S> for AuthAuditApp {
    fn id(&self) -> &'static str {
        "authaudit"
    }

    fn register(&self, app: &mut AppBuilder<S>) {
        for migration in self.schema {
            app.schema(*migration);
        }
        app.model(auth_event_model());
    }
}

pub fn auth_event_model() -> Model {
    Model::new("authaudit.event")
        .table("authaudit_events")
        .field(Column::uuid("id").primary_key())
        .field(Column::timestamptz("occurred_at").default_now())
        .field(Column::text("kind").chars(1, 64))
        .field(Column::text("outcome").one_of(&["ok", "denied", "error"]))
        .field(Column::integer("status").at_least(0))
        .field(Column::text("email").nullable().chars(0, 254))
        .field(Column::uuid("actor_id").nullable())
        .field(Column::text("client").nullable().chars(0, 64))
        .field(Column::text("detail").nullable().chars(0, 500))
        .index("authaudit_events_time_idx", &["occurred_at"])
        .index("authaudit_events_actor_idx", &["actor_id", "occurred_at"])
}

#[derive(Default)]
pub struct AuthEvent<'a> {
    pub kind: &'a str,
    pub outcome: &'a str,
    pub status: u16,
    pub email: Option<&'a str>,
    pub actor: Option<ActorId>,
    pub client: Option<&'a str>,
    pub detail: Option<&'a str>,
}

pub async fn record(pool: &PgPool, event: AuthEvent<'_>) -> Result<(), sqlx::Error> {
    let clip = |s: Option<&str>, n: usize| s.map(|s| s.chars().take(n).collect::<String>());
    sqlx::query(
        "INSERT INTO authaudit_events (id, kind, outcome, status, email, actor_id, client, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7())
    .bind(event.kind)
    .bind(event.outcome)
    .bind(i32::from(event.status))
    .bind(clip(event.email, 254))
    .bind(event.actor.map(|a| a.0))
    .bind(clip(event.client, 64))
    .bind(clip(event.detail, 500))
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(serde::Serialize)]
pub struct AuditRow {
    pub occurred_at: String,
    pub kind: String,
    pub outcome: String,
    pub status: i32,
    pub email: Option<String>,
    pub actor: Option<Uuid>,
    pub client: Option<String>,
    pub detail: Option<String>,
}

/// The most recent events, newest first.
pub async fn recent(pool: &PgPool, limit: i64) -> Result<Vec<AuditRow>, sqlx::Error> {
    type Row = (
        String,
        String,
        String,
        i32,
        Option<String>,
        Option<Uuid>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), kind, outcome, status,
                email, actor_id, client, detail
           FROM authaudit_events ORDER BY occurred_at DESC LIMIT $1",
    )
    .bind(limit.clamp(1, 1000))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(occurred_at, kind, outcome, status, email, actor, client, detail)| AuditRow {
                occurred_at,
                kind,
                outcome,
                status,
                email,
                actor,
                client,
                detail,
            },
        )
        .collect())
}

fn kind_for(path: &str) -> Option<&'static str> {
    Some(match path {
        password::LOGIN_PATH => "password.login",
        password::SET_PATH => "password.set",
        password::CHANGE_PATH => "password.change",
        routes::LOGOUT_PATH => "logout",
        routes::CALLBACK_PATH => "oidc.callback",
        _ => return None,
    })
}

/// Configuration for [`layer`]: the pool and, behind a proxy, the header
/// carrying the client address (same as `TRESTLE_CLIENT_ADDRESS_HEADER`).
#[derive(Clone)]
pub struct AuditLayer {
    pub pool: PgPool,
    pub client_address_header: Option<String>,
}

const MAX_BODY: usize = 16 * 1024;

/// Middleware: `axum::middleware::from_fn_with_state(AuditLayer{..}, audit::layer)`.
pub async fn layer(State(cfg): State<AuditLayer>, request: Request, next: Next) -> Response {
    let Some(kind) = kind_for(request.uri().path()) else {
        return next.run(request).await;
    };
    let client = cfg
        .client_address_header
        .as_deref()
        .and_then(|h| request.headers().get(h))
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').next().unwrap_or("").trim().to_owned())
        .or_else(|| {
            request
                .extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|c| c.0.ip().to_string())
        });
    // Only sign-in names an email; read it, then hand the body on unchanged.
    let (request, email) = if kind == "password.login" {
        let (parts, body) = request.into_parts();
        let timeout = input::body_timeout(&parts.extensions);
        let bytes = match input::read_body(body, MAX_BODY, timeout).await {
            Ok(b) => b,
            Err(problem) => return problem.into_response(),
        };
        let email = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.get("email")
                    .and_then(|e| e.as_str())
                    .map(|e| e.trim().to_lowercase())
            });
        (Request::from_parts(parts, Body::from(bytes)), email)
    } else {
        (request, None)
    };
    let response = next.run(request).await;
    let status = response.status();
    let outcome = if status.is_success() || status.is_redirection() {
        "ok"
    } else if status.is_server_error() {
        "error"
    } else {
        "denied"
    };
    let event = AuthEvent {
        kind,
        outcome,
        status: status.as_u16(),
        email: email.as_deref(),
        actor: None,
        client: client.as_deref(),
        detail: None,
    };
    if let Err(err) = record(&cfg.pool, event).await {
        tracing::error!(error = %err, kind, "auth audit write failed");
    }
    response
}
