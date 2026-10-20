//! API keys for non-browser actors (agents, services, scripts).
//!
//! An optional app: install [`ApiKeysApp`] and its migration to use it.
//!
//! **A key carries its actor's full authority.** Identity yields only an
//! [`ActorId`]; tenant, role and policy are decided by each operation, so
//! a request made with a key can do exactly what its actor can do. To give
//! a script or agent less, issue the key to a *service actor* whose
//! memberships and roles (ordinary policies) allow only what it needs.
//!
//! * A key is `tk_` + 256 random bits (base64url). Only its SHA-256 is
//!   stored, with an 8-character display prefix; the secret is shown once,
//!   at issue.
//! * A key belongs to one actor and may carry **scope labels**: opaque
//!   strings (for example `place:crm/*`) that Trestle stores and returns
//!   but never enforces. They are not an authorization mechanism; only
//!   project code that reads [`KeyInfo::scopes`] and checks them itself
//!   gives them meaning.
//! * Keys can expire and be revoked; a revoked or expired key verifies as
//!   unknown, identically to a wrong key.
//! * [`ApiKeyIdentity`] is an [`IdentityAdapter`] that accepts
//!   `Authorization: Bearer tk_…`, so keys slot into a [`crate::Chain`].
//!   Because it yields only the actor, it **refuses keys that carry scope
//!   labels** (exactly like an unknown key), so a key labelled
//!   `lessons.read` can never silently act with its actor's write access.
//!   [`ApiKeyIdentity::accept_unenforced_scopes`] lifts that refusal for a
//!   project that knowingly treats scopes as labels only.

use std::time::Duration;

use http::request::Parts;
use sqlx::PgPool;
use trestle_core::identity::{ActorId, AuthError, BoxFuture, IdentityAdapter, bearer_token};
use trestle_core::registry::{App, AppBuilder};
use trestle_historical::MigrationRef;
use trestle_historical::schema::{Column, Model};
use uuid::Uuid;

use crate::random;

/// The secret's prefix; lets scanners and adapters recognise a key.
pub const KEY_PREFIX: &str = "tk_";

pub struct ApiKeysApp {
    schema: &'static [MigrationRef],
}

impl ApiKeysApp {
    pub const fn new(schema: &'static [MigrationRef]) -> Self {
        Self { schema }
    }
}

impl<S: Clone + Send + Sync + 'static> App<S> for ApiKeysApp {
    fn id(&self) -> &'static str {
        "apikeys"
    }

    fn register(&self, app: &mut AppBuilder<S>) {
        for migration in self.schema {
            app.schema(*migration);
        }
        app.model(api_key_model());
    }
}

pub fn api_key_model() -> Model {
    Model::new("apikeys.key")
        .table("apikeys_keys")
        .field(Column::uuid("id").primary_key())
        .field(Column::uuid("actor_id"))
        .field(Column::text("label").chars(1, 200))
        .field(Column::text("prefix").chars(8, 8))
        .field(Column::text("key_hash").chars(64, 64))
        .field(Column::text("scopes").chars(0, 4000))
        .field(Column::timestamptz("created_at").default_now())
        .field(Column::timestamptz("expires_at").nullable())
        .field(Column::timestamptz("revoked_at").nullable())
        .field(Column::timestamptz("last_used_at").nullable())
        .unique("apikeys_keys_key_hash_key", &["key_hash"])
        .index("apikeys_keys_actor_idx", &["actor_id"])
}

/// A verified key: who it acts for, and its scope labels (not enforced by
/// Trestle; see the module docs).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct KeyInfo {
    pub id: Uuid,
    pub actor: ActorId,
    pub label: String,
    pub scopes: Vec<String>,
}

/// A newly issued key. `secret` is shown once and never stored.
pub struct IssuedKey {
    pub id: Uuid,
    pub prefix: String,
    pub secret: String,
}

#[derive(Debug, serde::Serialize)]
pub struct KeySummary {
    pub id: Uuid,
    pub actor: ActorId,
    pub label: String,
    pub prefix: String,
    pub scopes: Vec<String>,
    pub status: &'static str,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

fn check_scopes(scopes: &[String]) -> Result<String, String> {
    for s in scopes {
        if s.is_empty() || s.len() > 200 || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "invalid scope {s:?}: non-empty, no whitespace, ≤200 chars"
            ));
        }
    }
    Ok(scopes.join(" "))
}

fn split(scopes: &str) -> Vec<String> {
    scopes
        .split(' ')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Issue a key for `actor`. `valid_for: None` never expires.
///
/// `scopes` are labels only: the key still carries the actor's full
/// authority, and the default [`ApiKeyIdentity`] refuses a key that has
/// any. Pass `&[]` and restrict the actor instead.
pub async fn issue(
    pool: &PgPool,
    actor: ActorId,
    label: &str,
    scopes: &[String],
    valid_for: Option<Duration>,
) -> Result<IssuedKey, String> {
    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    issue_in(&mut conn, actor, label, scopes, valid_for).await
}

/// [`issue`] on the caller's connection or transaction.
pub async fn issue_in(
    conn: &mut sqlx::PgConnection,
    actor: ActorId,
    label: &str,
    scopes: &[String],
    valid_for: Option<Duration>,
) -> Result<IssuedKey, String> {
    let scopes = check_scopes(scopes)?;
    if label.trim().is_empty() || label.len() > 200 {
        return Err("label must be 1–200 characters".into());
    }
    let secret = format!("{KEY_PREFIX}{}", random::token());
    let prefix = secret[KEY_PREFIX.len()..KEY_PREFIX.len() + 8].to_owned();
    let id = Uuid::now_v7();
    let expires = valid_for.map(|d| d.as_secs() as f64);
    sqlx::query(
        "INSERT INTO apikeys_keys (id, actor_id, label, prefix, key_hash, scopes, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7::float8 IS NULL THEN NULL ELSE now() + make_interval(secs => $7) END)",
    )
    .bind(id)
    .bind(actor.0)
    .bind(label)
    .bind(&prefix)
    .bind(random::digest(&secret))
    .bind(scopes)
    .bind(expires)
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;
    Ok(IssuedKey { id, prefix, secret })
}

/// Shape check before any lookup, so arbitrary strings never reach the database.
pub fn looks_like_key(secret: &str) -> bool {
    secret
        .strip_prefix(KEY_PREFIX)
        .is_some_and(random::is_token)
}

/// The live key for `secret`, recording its use. `None` for unknown,
/// revoked or expired keys alike.
pub async fn verify(pool: &PgPool, secret: &str) -> Result<Option<KeyInfo>, sqlx::Error> {
    if !looks_like_key(secret) {
        return Ok(None);
    }
    let row: Option<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "UPDATE apikeys_keys SET last_used_at = now()
          WHERE key_hash = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())
          RETURNING id, actor_id, label, scopes",
    )
    .bind(random::digest(secret))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, actor, label, scopes)| KeyInfo {
        id,
        actor: ActorId(actor),
        label,
        scopes: split(&scopes),
    }))
}

/// Revoke a key; true if it was live.
pub async fn revoke(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE apikeys_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Revoke every live key of an actor (for example when it is disabled).
pub async fn revoke_actor(pool: &PgPool, actor: ActorId) -> Result<u64, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    revoke_actor_in(&mut conn, actor).await
}

/// [`revoke_actor`] on the caller's connection or transaction.
pub async fn revoke_actor_in(
    conn: &mut sqlx::PgConnection,
    actor: ActorId,
) -> Result<u64, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE apikeys_keys SET revoked_at = now() WHERE actor_id = $1 AND revoked_at IS NULL",
    )
    .bind(actor.0)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// All keys (never their secrets), newest first; optionally for one actor.
pub async fn list(pool: &PgPool, actor: Option<ActorId>) -> Result<Vec<KeySummary>, sqlx::Error> {
    type Row = (
        Uuid,
        Uuid,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, actor_id, label, prefix, scopes,
                CASE WHEN revoked_at IS NOT NULL THEN 'revoked'
                     WHEN expires_at IS NOT NULL AND expires_at <= now() THEN 'expired' ELSE 'active' END,
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                to_char(last_used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
           FROM apikeys_keys WHERE $1::uuid IS NULL OR actor_id = $1 ORDER BY created_at DESC",
    )
    .bind(actor.map(|a| a.0))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, actor, label, prefix, scopes, status, created_at, last_used_at)| KeySummary {
                id,
                actor: ActorId(actor),
                label,
                prefix,
                scopes: split(&scopes),
                status: match status.as_str() {
                    "revoked" => "revoked",
                    "expired" => "expired",
                    _ => "active",
                },
                created_at,
                last_used_at,
            },
        )
        .collect())
}

/// `Authorization: Bearer tk_…` → the key's actor.
///
/// The adapter yields only the actor, so the key acts with the actor's full
/// authority. By default it refuses any key that carries scope labels, with
/// the same [`AuthError::Invalid`] as an unknown key: a label such as
/// `lessons.read` must never look like a restriction while granting the
/// actor's write access. See [`Self::accept_unenforced_scopes`].
pub struct ApiKeyIdentity {
    pool: PgPool,
    accept_unenforced_scopes: bool,
}

impl ApiKeyIdentity {
    /// Accepts only keys without scope labels.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            accept_unenforced_scopes: false,
        }
    }

    /// Also accept keys that carry scope labels, **without enforcing them**:
    /// such a key still acts with its actor's full authority. Use this only
    /// when the labels are informational, or when the project's own code
    /// checks them on every path a key can reach.
    pub fn accept_unenforced_scopes(mut self) -> Self {
        self.accept_unenforced_scopes = true;
        self
    }
}

impl IdentityAdapter for ApiKeyIdentity {
    fn authenticate<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Result<ActorId, AuthError>> {
        Box::pin(async move {
            let token = bearer_token(parts)?;
            if !looks_like_key(token) {
                return Err(AuthError::Invalid);
            }
            match verify(&self.pool, token).await {
                Ok(Some(key)) if key.scopes.is_empty() || self.accept_unenforced_scopes => {
                    Ok(key.actor)
                }
                Ok(Some(key)) => {
                    tracing::warn!(
                        key = %key.id,
                        "API key with scope labels refused: ApiKeyIdentity does not enforce scopes"
                    );
                    Err(AuthError::Invalid)
                }
                Ok(None) => Err(AuthError::Invalid),
                Err(err) => {
                    tracing::warn!(error = %err, "API key lookup failed");
                    Err(AuthError::Unavailable)
                }
            }
        })
    }

    fn production_safe(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "api-key"
    }
}
