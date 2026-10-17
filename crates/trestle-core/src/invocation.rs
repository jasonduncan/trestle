//! What a caller's transport established, before any application lookup
//! (ADR-013).
//!
//! HTTP builds an [`Invocation`] from request parts ([`Invocation::from_http`]);
//! jobs, operator commands and tests build one directly. Operation contexts
//! resolve from it ([`FromInvocation`]), so every caller runs the same
//! membership and role lookup.

use std::future::Future;

use axum::extract::{FromRef, FromRequestParts};
use http::request::Parts;
use uuid::Uuid;

use crate::Problem;
use crate::identity::{ActorId, Authenticated, Identity};
use crate::operation::{ContextDescriptor, RequestContext};

/// Selects the tenant to act in. A selection grants nothing: contexts check
/// the caller's membership.
pub const TENANT_HEADER: &str = "trestle-tenant";

/// The caller and its selections. Fields are private so later additions do
/// not break callers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Invocation {
    actor: Option<ActorId>,
    tenant: Option<Uuid>,
    idempotency_key: Option<String>,
}

impl Invocation {
    /// No caller: only contexts that need no actor resolve.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// Acting as `actor`. Trusted: for jobs that recorded the actor who
    /// enqueued them, operator commands and tests. HTTP never uses it; a
    /// request's actor comes only from its credentials.
    pub fn for_actor(actor: ActorId) -> Self {
        Self {
            actor: Some(actor),
            tenant: None,
            idempotency_key: None,
        }
    }

    /// Selects a tenant. Contexts still check membership.
    pub fn with_tenant(mut self, tenant: Uuid) -> Self {
        self.tenant = Some(tenant);
        self
    }

    /// The caller, or 401 `credentials_missing`.
    pub fn actor(&self) -> Result<ActorId, Problem> {
        self.actor
            .ok_or_else(|| Problem::unauthenticated("credentials_missing"))
    }

    /// The selected tenant, if any.
    pub fn tenant(&self) -> Option<Uuid> {
        self.tenant
    }

    /// Makes a write repeatable (ADR-016): a repeat with the same key and
    /// input replays the committed answer. Honored only by operations that
    /// accept a key, through managed routes and `Registry::invoke`.
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    /// Reads what `descriptor` declares, and nothing else: credentials only
    /// when it is `authenticated` (through [`Authenticated`], so the identity
    /// chain, actor admission and per-request cache apply), and
    /// `Trestle-Tenant` only when it lists that header. A malformed tenant is
    /// 400 `tenant_invalid`; a missing one is left for the context to judge.
    pub async fn from_http<S>(
        parts: &mut Parts,
        state: &S,
        descriptor: &ContextDescriptor,
    ) -> Result<Self, Problem>
    where
        Identity: FromRef<S>,
        S: Send + Sync,
    {
        let actor = if descriptor.authenticated {
            Some(Authenticated::from_request_parts(parts, state).await?.0)
        } else {
            None
        };
        let declared = descriptor
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case(TENANT_HEADER));
        let tenant = match parts.headers.get(TENANT_HEADER) {
            Some(value) if declared => Some(
                value
                    .to_str()
                    .ok()
                    .and_then(|v| Uuid::parse_str(v.trim()).ok())
                    .ok_or_else(|| {
                        Problem::malformed("tenant_invalid", "Trestle-Tenant must be a tenant ID.")
                    })?,
            ),
            _ => None,
        };
        Ok(Self {
            actor,
            tenant,
            idempotency_key: None,
        })
    }
}

/// An operation context built from an [`Invocation`]: the application's
/// lookup (membership, role), the same for every caller.
pub trait FromInvocation<S>: RequestContext + Sized {
    fn resolve(
        invocation: &Invocation,
        state: &S,
    ) -> impl Future<Output = Result<Self, Problem>> + Send;
}

/// Axum extractor for hand-written handlers (I13): builds the caller's
/// [`Invocation`] from what `C` declares and resolves `C` from it, exactly as
/// managed routes do.
#[derive(Debug, Clone, Copy)]
pub struct Resolved<C>(pub C);

impl<S, C> FromRequestParts<S> for Resolved<C>
where
    C: FromInvocation<S>,
    Identity: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Problem> {
        let invocation = Invocation::from_http(parts, state, &C::describe()).await?;
        Ok(Resolved(C::resolve(&invocation, state).await?))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::{StatusCode, header};

    use super::*;
    use crate::identity::DevTokenIdentity;

    const ALICE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const TENANT: &str = "11111111-1111-4111-8111-111111111111";

    #[derive(Clone)]
    struct State(Identity);

    impl FromRef<State> for Identity {
        fn from_ref(state: &State) -> Self {
            state.0.clone()
        }
    }

    fn state() -> State {
        State(Arc::new(
            DevTokenIdentity::parse(&format!("alice-dev-token={ALICE}")).unwrap(),
        ))
    }

    fn parts(headers: &[(&str, &str)]) -> Parts {
        let mut builder = http::Request::builder();
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn tenant_context() -> ContextDescriptor {
        ContextDescriptor::authenticated().header("Trestle-Tenant", "Tenant.", Some("uuid"))
    }

    #[tokio::test]
    async fn declared_credentials_and_tenant_are_read() {
        let mut parts = parts(&[
            ("authorization", "Bearer alice-dev-token"),
            ("trestle-tenant", TENANT),
        ]);
        let invocation = Invocation::from_http(&mut parts, &state(), &tenant_context())
            .await
            .unwrap();
        assert_eq!(invocation.actor().unwrap().to_string(), ALICE);
        assert_eq!(invocation.tenant().unwrap().to_string(), TENANT);
        assert_eq!(
            invocation,
            Invocation::for_actor(invocation.actor().unwrap())
                .with_tenant(Uuid::parse_str(TENANT).unwrap()),
            "HTTP and trusted callers build the same value"
        );
    }

    #[tokio::test]
    async fn undeclared_inputs_are_never_read() {
        // Bad credentials and a malformed tenant would both be errors if read.
        let mut parts = parts(&[
            (header::AUTHORIZATION.as_str(), "Bearer wrong-token-here"),
            ("trestle-tenant", "not-a-uuid"),
        ]);
        let invocation = Invocation::from_http(&mut parts, &state(), &ContextDescriptor::default())
            .await
            .unwrap();
        assert_eq!(invocation, Invocation::anonymous());
        assert_eq!(
            invocation.actor().unwrap_err().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn declared_inputs_are_checked() {
        let missing = Invocation::from_http(&mut parts(&[]), &state(), &tenant_context())
            .await
            .unwrap_err();
        assert_eq!(missing.code(), "credentials_missing");

        let mut malformed = parts(&[
            ("authorization", "Bearer alice-dev-token"),
            ("trestle-tenant", "not-a-uuid"),
        ]);
        let problem = Invocation::from_http(&mut malformed, &state(), &tenant_context())
            .await
            .unwrap_err();
        assert_eq!(
            (problem.status(), problem.code()),
            (StatusCode::BAD_REQUEST, "tenant_invalid")
        );

        // No tenant is not an error here: the context decides whether it needs one.
        let mut none = parts(&[("authorization", "Bearer alice-dev-token")]);
        let invocation = Invocation::from_http(&mut none, &state(), &tenant_context())
            .await
            .unwrap();
        assert_eq!(invocation.tenant(), None);
    }
}
