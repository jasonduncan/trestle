//! The pilot product: the first real application built on Trestle.
//!
//! [`project`] is the composition root: the installed apps, in any order;
//! assembly derives the lifecycle order from their declared dependencies.

#![forbid(unsafe_code)]

pub mod audit;
pub mod commands;
pub mod config;
pub mod dev;
pub mod lessons;
pub mod mail;
pub mod provision;
pub mod schools;
pub mod server;
pub mod units;

use axum::Router;
use axum::extract::FromRef;
use sqlx::PgPool;
use trestle_core::http::Perimeter;
use trestle_core::identity::Identity;
use trestle_core::registry::{App, AssemblyError, Project, Registry};
use trestle_historical::planner::{self, Plan, PlanOptions};
use trestle_historical::{HistoryError, MigrationSet};
use trestle_identity::{Browser, IdentityApp};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub identity: Identity,
    /// Browser login routes; empty unless OIDC is configured.
    pub browser: Browser,
}

impl AppState {
    pub fn new(pool: PgPool, identity: Identity) -> Self {
        Self {
            pool,
            identity,
            browser: Browser::default(),
        }
    }
}

impl FromRef<AppState> for Browser {
    fn from_ref(state: &AppState) -> Self {
        state.browser.clone()
    }
}

impl FromRef<AppState> for Identity {
    fn from_ref(state: &AppState) -> Self {
        state.identity.clone()
    }
}

impl FromRef<AppState> for PgPool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

pub const NAME: &str = "Pilot API";

/// The installed apps, in any order.
pub fn apps() -> Vec<Box<dyn App<AppState>>> {
    vec![
        Box::new(IdentityApp::new(&[
            pilot_migrations::IDENTITY_0001,
            pilot_migrations::IDENTITY_0002,
        ])),
        Box::new(schools::SchoolsApp),
        Box::new(audit::AuditApp),
        Box::new(units::UnitsApp),
        Box::new(lessons::LessonsApp),
    ]
}

pub fn project() -> Project<AppState> {
    apps().into_iter().fold(
        Project::new(NAME, env!("CARGO_PKG_VERSION")),
        Project::install,
    )
}

/// The `pilot` command line: Trestle's built-in commands plus pilot's own
/// (`provision`, `seed-dev`, and `account`/`mail` with pilot's wording).
pub fn cli() -> trestle::Project<AppState, server::PilotServe, commands::PilotCommand> {
    apps()
        .into_iter()
        .fold(
            trestle::Project::<AppState>::with_state().named(NAME, env!("CARGO_PKG_VERSION")),
            trestle::Project::install_app,
        )
        .without_intent()
        .embedded_migrations(pilot_migrations::files())
        .migrations_dir(pilot_migrations::artifact_dir())
        .serve_with::<server::PilotServe>()
        // An app that used to live here moved to another project;
        // databases that ran it keep its rows.
        .foreign_apps(pilot_migrations::FOREIGN.iter().copied())
        .commands(commands::run)
}

/// Assembles the registry. Pure and offline: no database or network.
pub fn registry() -> Result<Registry<AppState>, AssemblyError> {
    project().assemble()
}

/// Compares the installed models (desired) with committed history. Pure
/// and offline, like `makemigrations`.
pub fn migration_plan(
    registry: &Registry<AppState>,
    mut options: PlanOptions,
) -> Result<(MigrationSet, Plan), HistoryError> {
    let set = pilot_migrations::history()?;
    let history = set.history()?;
    options.backfills.extend(registry.backfills().clone());
    let plan = planner::diff(&history, registry.desired_schema(), &options);
    Ok((set, plan))
}

pub fn router(registry: &Registry<AppState>, state: AppState) -> Router {
    router_with(registry, state, Perimeter::default())
}

/// The router with perimeter controls (admission, rate limits) as served.
pub fn router_with(registry: &Registry<AppState>, state: AppState, perimeter: Perimeter) -> Router {
    trestle_core::http::finish_with(registry.router().with_state(state), perimeter)
}
