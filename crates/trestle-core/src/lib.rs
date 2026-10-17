//! Trestle core.
//!
//! * [`operation`]: the operation contract and the executor every caller uses
//! * [`registry`]: apps, models, pure assembly, managed routes, exports
//! * [`query`]: a small typed query surface that requires a scope
//! * [`identity`], [`input`], [`problem`], [`db`], [`request_id`]: the M0
//!   request path, unchanged in behavior
//!
//! Portable formats (Contract IR, JSON Schema, interaction descriptors) live
//! in `trestle-protocol` and are re-exported as [`protocol`].

#![forbid(unsafe_code)]

pub mod admission;
pub mod db;
mod health;
pub mod http;
pub mod identity;
pub mod input;
pub mod invocation;
pub mod jobs;
pub mod metrics;
pub mod operation;
pub mod pagination;
pub mod patch;
pub mod problem;
pub mod query;
pub mod registry;
pub mod request_id;

pub use problem::{FieldError, Problem};
pub use trestle_protocol as protocol;
