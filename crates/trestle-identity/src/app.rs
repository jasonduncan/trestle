//! The `identity` app: session models and login routes.

use axum::extract::FromRef;
use axum::routing::{get, post};
use http::Method;
use trestle_core::registry::{App, AppBuilder};
use trestle_historical::MigrationRef;
use trestle_historical::schema::{Column, Model};

use crate::password;
use crate::routes::{
    self, Browser, CALLBACK_PATH, LOGIN_PATH, LOGOUT_PATH, METHODS_PATH, SESSION_PATH,
};

/// Installs identity persistence and the browser login routes. The
/// project's migration crate owns the generated artifacts for these
/// models; pass their references here.
pub struct IdentityApp {
    schema: &'static [MigrationRef],
}

impl IdentityApp {
    pub const fn new(schema: &'static [MigrationRef]) -> Self {
        Self { schema }
    }
}

const WAIVER: &str = "Identity integration (ADR-005): browser redirects and cookies, not domain operations. \
Reviewed 2026-09-26.";

impl<S> App<S> for IdentityApp
where
    S: Clone + Send + Sync + 'static,
    Browser: FromRef<S>,
{
    fn id(&self) -> &'static str {
        "identity"
    }

    fn register(&self, app: &mut AppBuilder<S>) {
        for migration in self.schema {
            app.schema(*migration);
        }
        app.model(session_model())
            .model(login_model())
            .model(account_model())
            .model(account_token_model());
        app.unmanaged(Method::GET, LOGIN_PATH, get(routes::login), WAIVER)
            .unmanaged(Method::GET, CALLBACK_PATH, get(routes::callback), WAIVER)
            .unmanaged(Method::GET, SESSION_PATH, get(routes::session), WAIVER)
            .unmanaged(Method::POST, LOGOUT_PATH, post(routes::logout), WAIVER)
            .unmanaged(Method::GET, METHODS_PATH, get(routes::methods), WAIVER)
            .unmanaged(
                Method::POST,
                password::LOGIN_PATH,
                post(password::login),
                WAIVER,
            )
            .unmanaged(
                Method::POST,
                password::INSPECT_PATH,
                post(password::inspect),
                WAIVER,
            )
            .unmanaged(
                Method::POST,
                password::SET_PATH,
                post(password::set),
                WAIVER,
            )
            .unmanaged(
                Method::POST,
                password::CHANGE_PATH,
                post(password::change),
                WAIVER,
            );
    }
}

pub fn session_model() -> Model {
    Model::new("identity.session")
        .table("identity_sessions")
        .field(Column::uuid("id").primary_key())
        .field(Column::text("token_hash").chars(64, 64))
        .field(Column::uuid("actor_id"))
        .field(Column::timestamptz("created_at").default_now())
        .field(Column::timestamptz("expires_at"))
        .field(Column::timestamptz("last_seen_at").default_now())
        .field(Column::timestamptz("revoked_at").nullable())
        .unique("identity_sessions_token_hash_key", &["token_hash"])
        .index("identity_sessions_actor_idx", &["actor_id"])
        .index("identity_sessions_expires_idx", &["expires_at"])
}

pub fn login_model() -> Model {
    Model::new("identity.login")
        .table("identity_logins")
        .field(Column::uuid("id").primary_key())
        .field(Column::text("state_hash").chars(64, 64))
        .field(Column::text("nonce").chars(43, 43))
        .field(Column::text("verifier").chars(43, 43))
        .field(Column::text("return_to").chars(1, 2048))
        .field(Column::timestamptz("created_at").default_now())
        .field(Column::timestamptz("expires_at"))
        .unique("identity_logins_state_hash_key", &["state_hash"])
        .index("identity_logins_expires_idx", &["expires_at"])
}

/// A password account. Created by an operator; the person sets the
/// password through a one-time link. The actor ID is the account's own.
pub fn account_model() -> Model {
    Model::new("identity.account")
        .table("identity_accounts")
        .field(Column::uuid("id").primary_key())
        .field(Column::uuid("actor_id"))
        .field(Column::text("email").chars(3, 254))
        .field(Column::text("password_hash").nullable().chars(1, 512))
        .field(Column::integer("failed_logins").at_least(0).default_int(0))
        .field(Column::timestamptz("locked_until").nullable())
        .field(Column::timestamptz("disabled_at").nullable())
        .field(Column::timestamptz("password_changed_at").nullable())
        .field(Column::timestamptz("created_at").default_now())
        .unique("identity_accounts_email_key", &["email"])
        .unique("identity_accounts_actor_key", &["actor_id"])
}

/// An invite or reset link, stored as the SHA-256 of its secret.
pub fn account_token_model() -> Model {
    Model::new("identity.account_token")
        .table("identity_account_tokens")
        .field(Column::uuid("id").primary_key())
        .field(Column::uuid("account_id").references("identity.account"))
        .field(Column::text("purpose").one_of(&["invite", "reset"]))
        .field(Column::text("token_hash").chars(64, 64))
        .field(Column::timestamptz("created_at").default_now())
        .field(Column::timestamptz("expires_at"))
        .field(Column::timestamptz("used_at").nullable())
        .unique("identity_account_tokens_token_hash_key", &["token_hash"])
        .index("identity_account_tokens_account_idx", &["account_id"])
}
