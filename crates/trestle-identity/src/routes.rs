//! Browser login: authorization code flow with PKCE, state and nonce
//! (OpenID Connect Core §3.1, RFC 7636, RFC 9700).
//!
//! ```text
//! GET  /api/v1/auth/login?return_to=/path  -> 303 to the provider
//! GET  /api/v1/auth/callback?code&state    -> 303 to return_to, session cookie set
//! GET  /api/v1/auth/session                -> {actor, csrf_token, expires_at} | 401
//! GET  /api/v1/auth/methods                -> {password, oidc}
//! POST /api/v1/auth/logout                 -> 204, session revoked
//! ```
//!
//! Password sign-in and invite/reset links are in [`crate::password`].
//!
//! The login cookie carries the `state` so a callback only completes in the
//! browser that started it (login CSRF). `return_to` is a same-origin path;
//! absolute and scheme-relative URLs are refused (open redirects).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use trestle_core::Problem;
use trestle_core::identity::AuthError;

use crate::actor_for;
use crate::oidc::{OidcError, Provider, printable};
use crate::password::Passwords;
use crate::random;
use crate::session::{SESSION_COOKIE, SameSite, Sessions, clear_cookie, cookie, set_cookie};

pub const LOGIN_PATH: &str = "/api/v1/auth/login";
pub const CALLBACK_PATH: &str = "/api/v1/auth/callback";
pub const SESSION_PATH: &str = "/api/v1/auth/session";
pub const LOGOUT_PATH: &str = "/api/v1/auth/logout";
pub const METHODS_PATH: &str = "/api/v1/auth/methods";

pub const LOGIN_COOKIE: &str = "__Host-trestle_login";
const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);

/// The relying-party registration at the provider.
#[derive(Clone)]
pub struct Client {
    pub id: String,
    pub secret: Option<String>,
    /// Absolute callback URL registered with the provider, ending in
    /// [`CALLBACK_PATH`].
    pub redirect_url: String,
    /// Space-separated; must include `openid`.
    pub scopes: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("id", &self.id)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("redirect_url", &self.redirect_url)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// An OpenID Connect provider and this application's registration there.
pub struct OidcLogin {
    pub provider: Arc<Provider>,
    pub client: Client,
}

/// Everything the browser routes need: the session store and whichever
/// ways of signing in are configured.
pub struct BrowserAuth {
    pub sessions: Arc<Sessions>,
    pub oidc: Option<OidcLogin>,
    pub passwords: Option<Arc<Passwords>>,
}

/// Application state for the browser routes. `None` when browser sessions
/// are not configured (development tokens only); the routes then answer 404.
#[derive(Clone, Default)]
pub struct Browser(pub Option<Arc<BrowserAuth>>);

impl Browser {
    pub(crate) fn get(&self) -> Result<&BrowserAuth, Problem> {
        self.0.as_deref().ok_or_else(Problem::not_found)
    }
}

/// `Set-Cookie` for a new session.
pub(crate) fn session_cookie(sessions: &Sessions, token: &str) -> HeaderValue {
    set_cookie(
        SESSION_COOKIE,
        token,
        sessions.config().lifetime,
        SameSite::Strict,
    )
}

fn login_failed(code: &str) -> Problem {
    Problem::unauthenticated(code)
        .with_detail("Sign-in did not complete. Start again from the application.")
}

/// Query parameters, refusing repeats (which would make the value
/// ambiguous between components).
fn params(parts: &Parts) -> Result<HashMap<String, String>, Problem> {
    let mut out = HashMap::new();
    for (name, value) in url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes()) {
        if out.insert(name.into_owned(), value.into_owned()).is_some() {
            return Err(Problem::malformed(
                "conflicting_input",
                "A query parameter was given more than once.",
            ));
        }
    }
    Ok(out)
}

/// A same-origin path to return to after login.
pub fn safe_return_to(raw: Option<&str>) -> Result<String, Problem> {
    let invalid = || {
        Problem::malformed(
            "invalid_return_to",
            "return_to must be a path on this site, such as /lessons.",
        )
    };
    let Some(path) = raw else {
        return Ok("/".into());
    };
    if path.len() > 2048
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let base = url::Url::parse("https://origin.invalid/").expect("static URL");
    let joined = base.join(path).map_err(|_| invalid())?;
    if joined.origin() != base.origin() {
        return Err(invalid());
    }
    Ok(path.to_owned())
}

pub(crate) async fn login(
    State(browser): State<Browser>,
    parts: Parts,
) -> Result<Response, Problem> {
    let auth = browser.get()?;
    let login = auth.oidc.as_ref().ok_or_else(Problem::not_found)?;
    let params = params(&parts)?;
    let return_to = safe_return_to(params.get("return_to").map(String::as_str))?;
    let state = random::token();
    let nonce = random::token();
    let verifier = random::token();
    auth.sessions
        .begin_login(&state, &nonce, &verifier, &return_to)
        .await?;
    let challenge = random::pkce_challenge(&verifier);
    let location = login.provider.authorization_url(&[
        ("response_type", "code"),
        ("client_id", &login.client.id),
        ("redirect_uri", &login.client.redirect_url),
        ("scope", &login.client.scopes),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(location.as_str()).map_err(|_| Problem::internal())?,
    );
    // Lax: the provider's redirect back is a cross-site top-level GET.
    headers.insert(
        header::SET_COOKIE,
        set_cookie(LOGIN_COOKIE, &state, LOGIN_TTL, SameSite::Lax),
    );
    Ok((StatusCode::SEE_OTHER, headers).into_response())
}

pub(crate) async fn callback(
    State(browser): State<Browser>,
    parts: Parts,
) -> Result<Response, Problem> {
    let auth = browser.get()?;
    let login = auth.oidc.as_ref().ok_or_else(Problem::not_found)?;
    let params = params(&parts)?;
    let state = params
        .get("state")
        .filter(|s| random::is_token(s))
        .ok_or_else(|| login_failed("login_state_invalid"))?;
    let bound =
        cookie(&parts.headers, LOGIN_COOKIE).ok_or_else(|| login_failed("login_state_invalid"))?;
    // Compare digests: equal-length, and timing reveals nothing usable.
    if random::digest(bound) != random::digest(state) {
        return Err(login_failed("login_state_invalid"));
    }
    let pending = auth
        .sessions
        .take_login(state)
        .await?
        .ok_or_else(|| login_failed("login_expired"))?;

    if let Some(error) = params.get("error") {
        tracing::info!(error = %printable(error), "provider refused sign-in");
        return Err(login_failed("login_denied"));
    }
    let code = params
        .get("code")
        .filter(|c| !c.is_empty() && c.len() <= 2048)
        .ok_or_else(|| login_failed("login_failed"))?;
    let id_token = login
        .provider
        .exchange_code(
            &login.client.id,
            login.client.secret.as_deref(),
            code,
            &login.client.redirect_url,
            &pending.verifier,
        )
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "authorization code exchange failed");
            match err {
                OidcError::Unavailable(_) => Problem::unavailable("identity_unavailable"),
                _ => login_failed("login_failed"),
            }
        })?;
    let claims = login
        .provider
        .verify(&id_token, &login.client.id)
        .await
        .map_err(|err| match AuthError::from(err) {
            AuthError::Unavailable => Problem::unavailable("identity_unavailable"),
            _ => login_failed("login_failed"),
        })?;
    // OpenID Connect Core §3.1.3.7: nonce binds the token to this login;
    // with several audiences, azp must name this client.
    if claims.nonce.as_deref() != Some(pending.nonce.as_str())
        || (claims.aud.len() > 1 && claims.azp.as_deref() != Some(login.client.id.as_str()))
        || claims
            .azp
            .as_deref()
            .is_some_and(|azp| azp != login.client.id)
    {
        tracing::warn!("ID token nonce or authorized party mismatch");
        return Err(login_failed("login_failed"));
    }
    let actor = actor_for(&claims.iss, &claims.sub);
    // A new session per sign-in; any session this browser held ends.
    if let Some(previous) = cookie(&parts.headers, SESSION_COOKIE) {
        auth.sessions.revoke(previous).await?;
    }
    let token = auth.sessions.create(actor).await?;
    tracing::info!(%actor, "session created");

    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&pending.return_to).map_err(|_| Problem::internal())?,
    );
    headers.append(header::SET_COOKIE, session_cookie(&auth.sessions, &token));
    headers.append(
        header::SET_COOKIE,
        clear_cookie(LOGIN_COOKIE, SameSite::Lax),
    );
    Ok((StatusCode::SEE_OTHER, headers).into_response())
}

pub(crate) async fn session(
    State(browser): State<Browser>,
    parts: Parts,
) -> Result<Response, Problem> {
    let auth = browser.get()?;
    let (_, info) = auth.sessions.authenticate(&parts).await?;
    Ok(Json(info).into_response())
}

/// Which ways of signing in this deployment offers, for the sign-in screen.
pub(crate) async fn methods(State(browser): State<Browser>) -> Result<Response, Problem> {
    let auth = browser.get()?;
    Ok(Json(serde_json::json!({
        "password": auth.passwords.is_some(),
        "oidc": auth.oidc.is_some(),
    }))
    .into_response())
}

/// Idempotent: without a live session it only clears the cookie. A
/// cross-site request cannot log a user out (forgery checks apply).
pub(crate) async fn logout(
    State(browser): State<Browser>,
    parts: Parts,
) -> Result<Response, Problem> {
    let auth = browser.get()?;
    match auth.sessions.authenticate(&parts).await {
        Ok(_) => {
            let token = cookie(&parts.headers, SESSION_COOKIE).unwrap_or_default();
            auth.sessions.revoke(token).await?;
        }
        Err(AuthError::Missing | AuthError::Invalid) => {}
        Err(err) => return Err(err.into()),
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        clear_cookie(SESSION_COOKIE, SameSite::Strict),
    );
    Ok((StatusCode::NO_CONTENT, headers).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_is_a_same_site_path() {
        assert_eq!(safe_return_to(None).unwrap(), "/");
        for ok in ["/", "/lessons", "/lessons?id=1#top", "/a/../b"] {
            assert_eq!(safe_return_to(Some(ok)).unwrap(), ok);
        }
        for bad in [
            "https://evil.example.com/",
            "//evil.example.com/",
            "/\\evil.example.com",
            "\\\\evil",
            "lessons",
            "/lessons\r\nSet-Cookie: x=1",
            "javascript:alert(1)",
            "",
        ] {
            assert!(safe_return_to(Some(bad)).is_err(), "{bad:?}");
        }
        assert!(safe_return_to(Some(&format!("/{}", "a".repeat(3000)))).is_err());
    }
}
