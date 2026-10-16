//! Several credentials, one adapter.

use std::sync::Arc;

use http::header;
use http::request::Parts;
use trestle_core::identity::{ActorId, AuthError, BoxFuture, Identity, IdentityAdapter};

use crate::session::{SESSION_COOKIE, Sessions, cookie};

/// Bearer adapters for `Authorization` (tried in order), else the session
/// cookie. A request with an `Authorization` header never falls back to
/// its cookie: one request, one credential.
pub struct Chain {
    bearer: Vec<Identity>,
    sessions: Option<Arc<Sessions>>,
}

impl Chain {
    pub fn new(bearer: Vec<Identity>, sessions: Option<Arc<Sessions>>) -> Self {
        Self { bearer, sessions }
    }

    pub fn is_empty(&self) -> bool {
        self.bearer.is_empty() && self.sessions.is_none()
    }

    /// Names of the configured adapters, for boot logs.
    pub fn describe(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.bearer.iter().map(|b| b.name()).collect();
        if self.sessions.is_some() {
            names.push("browser-session");
        }
        names
    }
}

impl IdentityAdapter for Chain {
    fn authenticate<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Result<ActorId, AuthError>> {
        Box::pin(async move {
            if parts.headers.contains_key(header::AUTHORIZATION) {
                for adapter in &self.bearer {
                    match adapter.authenticate(parts).await {
                        Ok(actor) => return Ok(actor),
                        Err(AuthError::Invalid | AuthError::Missing) => continue,
                        Err(other) => return Err(other),
                    }
                }
                return Err(AuthError::Invalid);
            }
            match &self.sessions {
                Some(sessions) if cookie(&parts.headers, SESSION_COOKIE).is_some() => {
                    sessions.authenticate(parts).await.map(|(actor, _)| actor)
                }
                _ => Err(AuthError::Missing),
            }
        })
    }

    fn production_safe(&self) -> bool {
        self.bearer.iter().all(|b| b.production_safe())
    }

    fn name(&self) -> &'static str {
        "chain"
    }
}
