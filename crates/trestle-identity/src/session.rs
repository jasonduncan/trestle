//! Server-side browser sessions (design doc §12.2).
//!
//! * The cookie `__Host-trestle_session` holds 256 random bits; the
//!   database holds only their SHA-256. `Secure`, `HttpOnly`,
//!   `SameSite=Strict`, host-only, `Path=/`.
//! * Sessions have an absolute lifetime and an idle timeout, and can be
//!   revoked one at a time (logout) or per actor (operator command).
//! * Every login creates a new session: no identifier survives
//!   authentication.
//! * Unsafe methods (anything but GET/HEAD/OPTIONS) authenticated by the
//!   cookie must carry an allowed `Origin` and the session's CSRF token in
//!   `X-CSRF-Token`. The token is an HMAC of the session, so it is never
//!   stored and cannot be derived without the server key.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, Method, header};
use serde::Serialize;
use sha2::Sha256;
use sqlx::PgPool;
use trestle_core::identity::{ActorId, AuthError, BoxFuture, IdentityAdapter};
use uuid::Uuid;

use crate::oidc::{Transport, check_url};
use crate::random;

pub const SESSION_COOKIE: &str = "__Host-trestle_session";
pub const CSRF_HEADER: &str = "x-csrf-token";

#[derive(Clone)]
pub struct SessionConfig {
    key: Vec<u8>,
    allowed_origins: Vec<String>,
    /// Absolute lifetime from login.
    pub lifetime: Duration,
    /// Maximum time between requests.
    pub idle: Duration,
}

impl std::fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConfig")
            .field("key", &"<redacted>")
            .field("allowed_origins", &self.allowed_origins)
            .field("lifetime", &self.lifetime)
            .field("idle", &self.idle)
            .finish()
    }
}

impl SessionConfig {
    /// `key` signs CSRF tokens (at least 32 bytes). `origins` are the
    /// exact browser origins allowed to make cookie-authenticated unsafe
    /// requests, such as `https://app.example.com`.
    pub fn new(key: &[u8], origins: &[String], transport: Transport) -> Result<Self, String> {
        if key.len() < 32 {
            return Err("the session key must be at least 32 bytes".into());
        }
        if origins.is_empty() {
            return Err("at least one allowed origin is required".into());
        }
        let mut allowed_origins = Vec::new();
        for raw in origins {
            let url = check_url(raw, transport)?;
            if url.path() != "/" || url.query().is_some() {
                return Err(format!("{raw:?} is not an origin (no path or query)"));
            }
            allowed_origins.push(url.origin().ascii_serialization());
        }
        Ok(Self {
            key: key.to_vec(),
            allowed_origins,
            lifetime: Duration::from_secs(12 * 60 * 60),
            idle: Duration::from_secs(2 * 60 * 60),
        })
    }

    pub fn allowed_origins(&self) -> &[String] {
        &self.allowed_origins
    }
}

/// What `GET /api/v1/auth/session` reports to the browser.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub actor: ActorId,
    pub csrf_token: String,
    pub expires_at: String,
}

pub(crate) struct PendingLogin {
    pub nonce: String,
    pub verifier: String,
    pub return_to: String,
}

pub struct Sessions {
    pool: PgPool,
    config: SessionConfig,
}

type HmacSha256 = Hmac<Sha256>;

impl Sessions {
    pub fn new(pool: PgPool, config: SessionConfig) -> Arc<Self> {
        Arc::new(Self { pool, config })
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    fn mac(&self, token_hash: &str) -> HmacSha256 {
        let mut mac =
            HmacSha256::new_from_slice(&self.config.key).expect("HMAC takes any key size");
        mac.update(b"trestle.csrf/1\0");
        mac.update(token_hash.as_bytes());
        mac
    }

    fn csrf_token(&self, token_hash: &str) -> String {
        URL_SAFE_NO_PAD.encode(self.mac(token_hash).finalize().into_bytes())
    }

    /// The request's `Origin` must be exactly one of the allowed origins.
    /// Used for every unsafe cookie-authenticated request and for the
    /// sign-in endpoints, which have no session yet (login CSRF).
    pub fn check_origin(&self, headers: &HeaderMap) -> Result<(), AuthError> {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthError::Csrf)?;
        if self.config.allowed_origins.iter().any(|o| o == origin) {
            Ok(())
        } else {
            Err(AuthError::Csrf)
        }
    }

    /// Origin and CSRF checks for a cookie-authenticated request.
    fn check_forgery(&self, parts: &Parts, token_hash: &str) -> Result<(), AuthError> {
        if matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return Ok(());
        }
        self.check_origin(&parts.headers)?;
        let presented = parts
            .headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
            .ok_or(AuthError::Csrf)?;
        self.mac(token_hash)
            .verify_slice(&presented)
            .map_err(|_| AuthError::Csrf)
    }

    /// Creates a session for `actor`; returns the cookie secret.
    pub async fn create(&self, actor: ActorId) -> Result<String, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let token = self.create_in(&mut tx, actor).await?;
        tx.commit().await?;
        Ok(token)
    }

    /// [`Sessions::create`] inside the caller's transaction, so a session
    /// is only issued together with the checks that justify it.
    pub(crate) async fn create_in(
        &self,
        conn: &mut sqlx::PgConnection,
        actor: ActorId,
    ) -> Result<String, sqlx::Error> {
        let token = random::token();
        sqlx::query("DELETE FROM identity_sessions WHERE expires_at < now() - interval '1 day'")
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "INSERT INTO identity_sessions (id, token_hash, actor_id, expires_at, last_seen_at) \
             VALUES ($1, $2, $3, now() + make_interval(secs => $4), now())",
        )
        .bind(Uuid::now_v7())
        .bind(random::digest(&token))
        .bind(actor.0)
        .bind(self.config.lifetime.as_secs_f64())
        .execute(&mut *conn)
        .await?;
        Ok(token)
    }

    /// The live session for `token`, touching its idle clock at most once a
    /// minute.
    async fn find(&self, token: &str) -> Result<Option<(Uuid, ActorId, String)>, sqlx::Error> {
        let row: Option<(Uuid, Uuid, bool, String)> = sqlx::query_as(
            "SELECT id, actor_id, last_seen_at < now() - interval '60 seconds', \
                    to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             FROM identity_sessions \
             WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now() \
               AND last_seen_at > now() - make_interval(secs => $2)",
        )
        .bind(random::digest(token))
        .bind(self.config.idle.as_secs_f64())
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, actor, stale, expires_at)) = row else {
            return Ok(None);
        };
        if stale {
            sqlx::query("UPDATE identity_sessions SET last_seen_at = now() WHERE id = $1")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(Some((id, ActorId(actor), expires_at)))
    }

    /// Authenticates a request by its session cookie, including forgery
    /// checks for unsafe methods.
    pub async fn authenticate(&self, parts: &Parts) -> Result<(ActorId, SessionInfo), AuthError> {
        let token = cookie(&parts.headers, SESSION_COOKIE).ok_or(AuthError::Missing)?;
        if !random::is_token(token) {
            return Err(AuthError::Invalid);
        }
        let token_hash = random::digest(token);
        self.check_forgery(parts, &token_hash)?;
        let (_, actor, expires_at) = self
            .find(token)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "session lookup failed");
                AuthError::Unavailable
            })?
            .ok_or(AuthError::Invalid)?;
        Ok((
            actor,
            SessionInfo {
                actor,
                csrf_token: self.csrf_token(&token_hash),
                expires_at,
            },
        ))
    }

    /// What the browser learns about a session it was just given.
    pub(crate) async fn info_for(
        &self,
        actor: ActorId,
        token: &str,
    ) -> Result<SessionInfo, sqlx::Error> {
        let token_hash = random::digest(token);
        let expires_at: String = sqlx::query_scalar(
            "SELECT to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             FROM identity_sessions WHERE token_hash = $1",
        )
        .bind(&token_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(SessionInfo {
            actor,
            csrf_token: self.csrf_token(&token_hash),
            expires_at,
        })
    }

    /// Ends the session whose cookie secret is `token`.
    pub async fn revoke(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE identity_sessions SET revoked_at = now() \
             WHERE token_hash = $1 AND revoked_at IS NULL",
        )
        .bind(random::digest(token))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Ends every session of `actor` (for example after access is
    /// withdrawn at the provider). Returns how many were live.
    pub async fn revoke_actor(&self, actor: ActorId) -> Result<u64, sqlx::Error> {
        revoke_actor(&self.pool, actor).await
    }

    /// Ends every session of `actor` except the one whose cookie secret is
    /// `keep` (after a password change in that session).
    pub async fn revoke_others(&self, actor: ActorId, keep: &str) -> Result<u64, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        revoke_others_in(&mut conn, actor, keep).await
    }

    pub(crate) async fn begin_login(
        &self,
        state: &str,
        nonce: &str,
        verifier: &str,
        return_to: &str,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM identity_logins WHERE expires_at < now()")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO identity_logins (id, state_hash, nonce, verifier, return_to, expires_at) \
             VALUES ($1, $2, $3, $4, $5, now() + interval '10 minutes')",
        )
        .bind(Uuid::now_v7())
        .bind(random::digest(state))
        .bind(nonce)
        .bind(verifier)
        .bind(return_to)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    /// Consumes a pending login: each state is usable once.
    pub(crate) async fn take_login(
        &self,
        state: &str,
    ) -> Result<Option<PendingLogin>, sqlx::Error> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "DELETE FROM identity_logins WHERE state_hash = $1 AND expires_at > now() \
             RETURNING nonce, verifier, return_to",
        )
        .bind(random::digest(state))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(nonce, verifier, return_to)| PendingLogin {
            nonce,
            verifier,
            return_to,
        }))
    }
}

/// Revokes every live session of `actor`. Usable without a configured
/// provider (operator commands).
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
        "UPDATE identity_sessions SET revoked_at = now() \
         WHERE actor_id = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(actor.0)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// Ends every session of `actor` except the one whose cookie secret is
/// `keep`, on the caller's connection or transaction.
pub(crate) async fn revoke_others_in(
    conn: &mut sqlx::PgConnection,
    actor: ActorId,
    keep: &str,
) -> Result<u64, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE identity_sessions SET revoked_at = now() \
             WHERE actor_id = $1 AND revoked_at IS NULL AND expires_at > now() \
               AND token_hash <> $2",
    )
    .bind(actor.0)
    .bind(random::digest(keep))
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// The adapter form of [`Sessions`]: cookie in, actor out.
pub struct SessionIdentity(pub Arc<Sessions>);

impl IdentityAdapter for SessionIdentity {
    fn authenticate<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Result<ActorId, AuthError>> {
        Box::pin(async move { self.0.authenticate(parts).await.map(|(actor, _)| actor) })
    }

    fn production_safe(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "browser-session"
    }
}

/// The first value of cookie `name` across all `Cookie` headers.
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, value)| value)
}

#[derive(Clone, Copy)]
pub(crate) enum SameSite {
    Strict,
    Lax,
}

/// A host-only, secure, script-inaccessible cookie.
pub(crate) fn set_cookie(
    name: &str,
    value: &str,
    max_age: Duration,
    same_site: SameSite,
) -> HeaderValue {
    let same_site = match same_site {
        SameSite::Strict => "Strict",
        SameSite::Lax => "Lax",
    };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Secure; HttpOnly; SameSite={same_site}; Max-Age={}",
        max_age.as_secs()
    ))
    .expect("cookie values are base64url")
}

pub(crate) fn clear_cookie(name: &str, same_site: SameSite) -> HeaderValue {
    set_cookie(name, "", Duration::ZERO, same_site)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SessionConfig {
        SessionConfig::new(
            &[7u8; 32],
            &["https://app.example.com".to_owned()],
            Transport::HttpsOnly,
        )
        .unwrap()
    }

    #[test]
    fn configuration_is_validated() {
        let origins = ["https://app.example.com".to_owned()];
        assert!(SessionConfig::new(&[0u8; 16], &origins, Transport::HttpsOnly).is_err());
        assert!(SessionConfig::new(&[0u8; 32], &[], Transport::HttpsOnly).is_err());
        assert!(
            SessionConfig::new(
                &[0u8; 32],
                &["https://app.example.com/app".into()],
                Transport::HttpsOnly
            )
            .is_err()
        );
        assert!(
            SessionConfig::new(
                &[0u8; 32],
                &["http://app.example.com".into()],
                Transport::HttpsOnly
            )
            .is_err()
        );
        assert_eq!(config().allowed_origins(), ["https://app.example.com"]);
        assert!(
            !format!("{:?}", config()).contains("7, 7"),
            "key is redacted"
        );
    }

    #[test]
    fn cookies_are_found_across_headers() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1; theme=dark"));
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("__Host-trestle_session=abc; b=2"),
        );
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("abc"));
        assert_eq!(cookie(&headers, "theme"), Some("dark"));
        assert_eq!(cookie(&headers, "missing"), None);
    }

    #[tokio::test]
    async fn csrf_tokens_are_bound_to_session_and_key() {
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let sessions = Sessions::new(pool, config());
        let a = sessions.csrf_token("hash-a");
        assert_eq!(a, sessions.csrf_token("hash-a"));
        assert_ne!(a, sessions.csrf_token("hash-b"));

        let request = |method: Method, origin: Option<&str>, token: Option<&str>| {
            let mut builder = http::Request::builder().method(method);
            if let Some(origin) = origin {
                builder = builder.header(header::ORIGIN, origin);
            }
            if let Some(token) = token {
                builder = builder.header(CSRF_HEADER, token);
            }
            builder.body(()).unwrap().into_parts().0
        };
        let good = Some("https://app.example.com");
        assert!(
            sessions
                .check_forgery(&request(Method::GET, None, None), "hash-a")
                .is_ok()
        );
        assert!(
            sessions
                .check_forgery(&request(Method::POST, good, Some(&a)), "hash-a")
                .is_ok()
        );
        for (origin, token) in [
            (None, Some(a.as_str())),
            (Some("https://evil.example.com"), Some(a.as_str())),
            (Some("null"), Some(a.as_str())),
            (good, None),
            (good, Some("not-base64!")),
            (good, Some(sessions.csrf_token("hash-b").as_str())),
        ] {
            assert!(matches!(
                sessions.check_forgery(&request(Method::PATCH, origin, token), "hash-a"),
                Err(AuthError::Csrf)
            ));
        }
    }

    #[test]
    fn cookies_are_host_only_secure_and_http_only() {
        let value = set_cookie(
            SESSION_COOKIE,
            "abc",
            Duration::from_secs(60),
            SameSite::Strict,
        );
        assert_eq!(
            value.to_str().unwrap(),
            "__Host-trestle_session=abc; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=60"
        );
        assert!(
            clear_cookie(SESSION_COOKIE, SameSite::Strict)
                .to_str()
                .unwrap()
                .ends_with("Max-Age=0")
        );
    }
}
