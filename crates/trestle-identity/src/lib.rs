//! Identity integrations for Trestle (design doc §12.1–12.2, ADR-005).
//!
//! Identity is an integration; authorization stays in operation policies.
//! These adapters only answer "which actor is calling":
//!
//! * [`oidc`] — an OpenID Connect relying party: discovery, signing keys,
//!   token verification and the authorization-code exchange.
//! * [`session`] — server-side browser sessions with origin and CSRF
//!   checks, established by the login routes.
//! * [`password`] — password accounts (Argon2id), with operator-issued
//!   invite and reset links, signing in to the same sessions.
//! * [`bearer`] — signed JWT access tokens for API clients.
//! * [`Chain`] — bearer adapters first, then the session cookie.
//! * [`IdentityApp`] — installs the session models and login routes.
//! * [`api_keys`] — optional app: API keys for agents and services. A key
//!   carries its actor's full authority; scope labels are not enforced.
//! * [`audit`] — optional app: the sign-in audit trail and its middleware.
//!
//! Actors are derived from the provider account: [`actor_for`] is a
//! UUIDv5 of issuer and subject, so the same account is the same actor on
//! every replica without a lookup table. Changing providers changes actor
//! IDs; memberships must then be remapped (a documented pilot limit).

#![forbid(unsafe_code)]

pub mod api_keys;
mod app;
pub mod audit;
pub mod bearer;
mod chain;
#[cfg(test)]
mod lifecycle_tests;
pub mod oidc;
pub mod password;
mod random;
pub mod routes;
pub mod session;

pub use app::{IdentityApp, account_model, account_token_model, login_model, session_model};
pub use chain::Chain;
pub use routes::{Browser, BrowserAuth, Client, OidcLogin};

use trestle_core::identity::ActorId;
use uuid::Uuid;

/// The actor for provider account `subject` at `issuer`.
pub fn actor_for(issuer: &str, subject: &str) -> ActorId {
    let namespace = Uuid::new_v5(&Uuid::NAMESPACE_URL, b"urn:trestle:actor");
    // Length-prefixed so no issuer/subject pair can collide with another.
    let name = format!("{}:{issuer}{subject}", issuer.len());
    ActorId(Uuid::new_v5(&namespace, name.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actors_are_stable_and_distinct_per_account() {
        let a = actor_for("https://id.example.com", "user-1");
        assert_eq!(a, actor_for("https://id.example.com", "user-1"));
        assert_ne!(a, actor_for("https://id.example.com", "user-2"));
        assert_ne!(a, actor_for("https://other.example.com", "user-1"));
        assert_ne!(
            actor_for("https://id.example.com/a", "b"),
            actor_for("https://id.example.com/", "ab")
        );
        assert_eq!(a.0.get_version_num(), 5);
    }
}
