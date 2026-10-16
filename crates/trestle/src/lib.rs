//! Trestle: an API-first Rust application framework.
//!
//! This is the one crate a project depends on. The happy path is
//!
//! ```no_run
//! fn main() -> std::process::ExitCode {
//!     trestle::project().run()
//! }
//! ```
//!
//! [`project()`] returns a [`Project`] using the default [`State`]; install
//! apps that `impl trestle::App` (no type parameter) and call
//! [`Project::run`], which parses the command line (`serve`, `check`,
//! `migrate`, `makemigrations`, `openapi`, …) exactly like Django's
//! `manage.py`. Projects with their own state type use
//! [`Project::with_state`] and implement [`AppOf<S>`] instead.
//!
//! The building blocks are re-exported as [`core`], [`protocol`],
//! [`historical`], [`identity`] and [`mail`]. Those crates are internal:
//! only what this crate exposes carries a stability promise.

#![forbid(unsafe_code)]

mod app;
mod environment;
pub mod intent;
pub mod mail;
pub mod migrations;
mod project;
pub mod reconcile;
pub mod serve;
mod state;
#[cfg(feature = "testing")]
pub mod testing;

pub use app::{App, AppBuilder, AppOf, Erased};
pub use environment::Environment;
pub use project::{
    CommandContext, LinkArgs, NoCommands, NoExtension, Project, ServeExtension, connect, deliver,
    migration_plan, project,
};
pub use reconcile::{Failure, exit};
pub use serve::{ServeConfig, metrics_router};
pub use state::{State, StateParts};

pub use trestle_core as core;
pub use trestle_historical as historical;
pub use trestle_identity as identity;
pub use trestle_mail as mail_backend;
pub use trestle_protocol as protocol;

pub use trestle_core::Problem;
pub use trestle_core::registry::{AssemblyError, Registry};

#[doc(hidden)]
pub mod __private {
    pub use include_dir;
}

/// `Result` with [`Problem`] as the default error, as operations return.
pub type Result<T, E = Problem> = std::result::Result<T, E>;

/// The router a project serves, with perimeter controls.
pub fn router_with<S>(
    registry: &Registry<S>,
    state: S,
    perimeter: trestle_core::http::Perimeter,
) -> axum::Router
where
    S: Clone + Send + Sync + 'static,
    sqlx::PgPool: axum::extract::FromRef<S>,
{
    trestle_core::http::finish_with(registry.router().with_state(state), perimeter)
}

/// Everything a typical app module needs: `use trestle::prelude::*;`.
pub mod prelude {
    pub use crate::intent::{Ctx, Input, Row};
    pub use crate::{App, AppBuilder, Environment, Problem, Project, Result, State};
    pub use serde_json::json;
    pub use trestle_core::FieldError;
    pub use trestle_core::operation::{Mode, Operation, OperationInfo, Policy, RequestContext};
    pub use trestle_historical::MigrationRef;
    pub use trestle_historical::schema::{Column as Field, Model};
    pub use uuid::Uuid as Id;
}
