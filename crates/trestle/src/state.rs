//! The default application state, so the common case has no `<S>`.

use axum::extract::FromRef;
use sqlx::PgPool;
use trestle_core::identity::Identity;
use trestle_identity::Browser;

use crate::Environment;

/// What `serve` has built when it asks for the project's state.
pub struct StateParts {
    pub pool: PgPool,
    pub identity: Identity,
    /// Browser sign-in routes; empty unless sessions are configured.
    pub browser: Browser,
    pub env: Environment,
}

/// The default state handed to every operation and route.
#[derive(Clone)]
pub struct State {
    pub pool: PgPool,
    pub identity: Identity,
    pub browser: Browser,
    pub env: Environment,
}

impl State {
    /// State with no browser sign-in, e.g. for tests.
    pub fn new(pool: PgPool, identity: Identity) -> Self {
        Self {
            pool,
            identity,
            browser: Browser::default(),
            env: Environment::Development,
        }
    }
}

impl From<StateParts> for State {
    fn from(parts: StateParts) -> Self {
        Self {
            pool: parts.pool,
            identity: parts.identity,
            browser: parts.browser,
            env: parts.env,
        }
    }
}

impl FromRef<State> for PgPool {
    fn from_ref(state: &State) -> Self {
        state.pool.clone()
    }
}

impl FromRef<State> for Identity {
    fn from_ref(state: &State) -> Self {
        state.identity.clone()
    }
}

impl FromRef<State> for Browser {
    fn from_ref(state: &State) -> Self {
        state.browser.clone()
    }
}
