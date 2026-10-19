//! The intent file, `trestle.toml` (design §3.4): what the server is, as
//! data. The project binary reads it at startup (and `check` reads it with
//! no database) and builds the same registry the Rust builders build:
//! models with stable IDs, contracts, operations, routes and policies.
//! Rust handles only what the file can't say, through handlers and hooks
//! registered by name.

pub mod doc;
mod runtime;
pub mod spec;

pub use toml;

pub use doc::{
    AdminDoc, EndpointDoc, FieldDoc, IntentDoc, ModelDoc, ProjectDoc, TenancyDoc, ViewDoc,
    ViewSectionDoc,
};
pub use runtime::{
    Ctx, HandlerFn, Handlers, HookFn, Input, IntentApp, MAX_LIMIT, Permission, Resolved, Row,
    TENANT_HEADER,
};
pub use spec::{AUTHENTICATED, AuthSection, Intent, IntentError, Oidc, OidcProvider, parse};

/// Default intent file, relative to the working directory.
pub const DEFAULT_FILE: &str = "trestle.toml";
/// Environment variable naming another intent file.
pub const FILE_ENV: &str = "TRESTLE_INTENT";
