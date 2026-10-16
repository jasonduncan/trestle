//! Identity is an integration; authorization is core (design doc §12.1).
//!
//! An [`IdentityAdapter`] turns request credentials into a validated
//! [`ActorId`]. It never reads tenant, role or ownership from the request:
//! those come from verified membership after authentication.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use http::header;
use http::request::Parts;
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::Problem;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ActorId(pub Uuid);

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no credentials were presented")]
    Missing,
    #[error("the presented credentials are not valid")]
    Invalid,
    #[error("the identity provider is unavailable")]
    Unavailable,
    /// A cookie-authenticated unsafe request failed its origin or CSRF
    /// token check (§12.2).
    #[error("the request failed cross-site request forgery checks")]
    Csrf,
}

impl AuthError {
    /// Bounded metric label.
    pub fn reason(&self) -> &'static str {
        match self {
            AuthError::Missing => "missing",
            AuthError::Invalid => "invalid",
            AuthError::Unavailable => "unavailable",
            AuthError::Csrf => "csrf",
        }
    }
}

impl From<AuthError> for Problem {
    fn from(err: AuthError) -> Self {
        crate::metrics::increment(
            "trestle_auth_failures_total",
            "Requests refused while establishing identity, by reason.",
            &[("reason", err.reason())],
        );
        match err {
            AuthError::Missing => Problem::unauthenticated("credentials_missing"),
            AuthError::Invalid => Problem::unauthenticated("credentials_invalid"),
            AuthError::Unavailable => Problem::unavailable("identity_unavailable"),
            AuthError::Csrf => Problem::forbidden("csrf_failed"),
        }
    }
}

/// Establishes who is calling. Dyn-compatible so the composition root can
/// choose an adapter from configuration.
pub trait IdentityAdapter: Send + Sync + 'static {
    fn authenticate<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Result<ActorId, AuthError>>;

    /// Whether this adapter may run with `TRESTLE_ENV=production` (§12.4).
    fn production_safe(&self) -> bool;

    fn name(&self) -> &'static str;
}

pub type Identity = Arc<dyn IdentityAdapter>;

/// Extractor for an authenticated actor. Rejects with 401 problem+json.
#[derive(Debug, Clone, Copy)]
pub struct Authenticated(pub ActorId);

impl<S> FromRequestParts<S> for Authenticated
where
    Identity: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Problem> {
        if let Some(existing) = parts.extensions.get::<Authenticated>() {
            return Ok(*existing);
        }
        let identity = Identity::from_ref(state);
        let actor = identity.authenticate(parts).await?;
        crate::admission::charge_actor(&parts.extensions, actor.0)?;
        parts.extensions.insert(Authenticated(actor));
        Ok(Authenticated(actor))
    }
}

/// Reads `Authorization: Bearer <token>`.
pub fn bearer_token(parts: &Parts) -> Result<&str, AuthError> {
    let value = parts
        .headers
        .get(header::AUTHORIZATION)
        .ok_or(AuthError::Missing)?
        .to_str()
        .map_err(|_| AuthError::Invalid)?;
    let (scheme, token) = value.split_once(' ').ok_or(AuthError::Invalid)?;
    let token = token.trim();
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(AuthError::Invalid);
    }
    Ok(token)
}

/// Development-only adapter: fixed bearer tokens mapped to actors.
///
/// Configured as `token=actor-uuid,token=actor-uuid`. Tokens are held as
/// SHA-256 digests. Never production-safe: the service refuses to boot with
/// it when `TRESTLE_ENV=production`.
pub struct DevTokenIdentity {
    actors: HashMap<[u8; 32], ActorId>,
}

impl DevTokenIdentity {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut actors = HashMap::new();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (token, actor) = entry
                .split_once('=')
                .ok_or_else(|| "expected token=actor-uuid, got an entry without '='".to_owned())?;
            let token = token.trim();
            if token.len() < 8 {
                return Err("dev tokens must be at least 8 characters".into());
            }
            let actor = Uuid::parse_str(actor.trim())
                .map_err(|_| "actor for a dev token is not a UUID".to_owned())?;
            if actors.insert(digest(token), ActorId(actor)).is_some() {
                return Err("duplicate dev token".into());
            }
        }
        if actors.is_empty() {
            return Err("no dev tokens configured".into());
        }
        Ok(Self { actors })
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl IdentityAdapter for DevTokenIdentity {
    fn authenticate<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Result<ActorId, AuthError>> {
        Box::pin(async move {
            let token = bearer_token(parts)?;
            self.actors
                .get(&digest(token))
                .copied()
                .ok_or(AuthError::Invalid)
        })
    }

    fn production_safe(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "dev-tokens"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(authorization: Option<&str>) -> Parts {
        let mut builder = http::Request::builder();
        if let Some(value) = authorization {
            builder = builder.header(header::AUTHORIZATION, value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    const ALICE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    #[tokio::test]
    async fn dev_tokens_authenticate_known_bearer() {
        let identity = DevTokenIdentity::parse(&format!("alice-dev-token={ALICE}")).unwrap();
        let actor = identity
            .authenticate(&parts(Some("bearer alice-dev-token")))
            .await
            .unwrap();
        assert_eq!(actor.0.to_string(), ALICE);
    }

    #[tokio::test]
    async fn dev_tokens_reject_missing_unknown_and_wrong_scheme() {
        let identity = DevTokenIdentity::parse(&format!("alice-dev-token={ALICE}")).unwrap();
        assert!(matches!(
            identity.authenticate(&parts(None)).await,
            Err(AuthError::Missing)
        ));
        assert!(matches!(
            identity
                .authenticate(&parts(Some("Bearer nope-nope-nope")))
                .await,
            Err(AuthError::Invalid)
        ));
        assert!(matches!(
            identity
                .authenticate(&parts(Some("Basic alice-dev-token")))
                .await,
            Err(AuthError::Invalid)
        ));
    }

    #[test]
    fn dev_token_spec_is_validated() {
        assert!(DevTokenIdentity::parse("").is_err());
        assert!(DevTokenIdentity::parse("short=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").is_err());
        assert!(DevTokenIdentity::parse("long-enough-token=not-a-uuid").is_err());
        assert!(
            !DevTokenIdentity::parse(&format!("alice-dev-token={ALICE}"))
                .unwrap()
                .production_safe()
        );
    }
}
