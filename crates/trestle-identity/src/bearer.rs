//! `Authorization: Bearer <JWT>` for API clients (design doc §12.1).
//!
//! Accepts signed JWT access tokens (RFC 9068 style) issued by the
//! configured provider for this API's audience. Opaque access tokens would
//! need token introspection, which is not implemented. The API audience
//! must differ from the browser client ID, so an ID token issued to the
//! browser login can never be replayed as an API token.

use std::sync::Arc;

use http::request::Parts;
use trestle_core::identity::{ActorId, AuthError, BoxFuture, IdentityAdapter, bearer_token};

use crate::actor_for;
use crate::oidc::Provider;

pub struct BearerIdentity {
    provider: Arc<Provider>,
    audience: String,
}

impl BearerIdentity {
    pub fn new(provider: Arc<Provider>, audience: &str) -> Self {
        Self {
            provider,
            audience: audience.to_owned(),
        }
    }
}

impl IdentityAdapter for BearerIdentity {
    fn authenticate<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Result<ActorId, AuthError>> {
        Box::pin(async move {
            let token = bearer_token(parts)?;
            if token.split('.').count() != 3 {
                return Err(AuthError::Invalid);
            }
            let claims = self.provider.verify(token, &self.audience).await?;
            Ok(actor_for(&claims.iss, &claims.sub))
        })
    }

    fn production_safe(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "oidc-bearer"
    }
}
