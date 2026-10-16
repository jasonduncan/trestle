//! Operations and the executor (design doc §6).
//!
//! An [`Operation`] is the one place a behavior lives. Every caller (managed
//! HTTP route, hand-written Axum handler, test, later forms/admin/jobs) goes
//! through [`execute`] or [`execute_json`], which apply the same sequence:
//!
//! ```text
//! decode + portable validation (contract)
//!   -> operation-level authorization (Policy::admit)
//!   -> Write: begin transaction -> Policy::recheck -> run -> commit (classified)
//!      Read:  acquire connection -> run
//! ```
//!
//! The executor owns the transaction. Operations receive an explicit
//! connection and never touch the pool.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::request::Parts;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::pool::PoolConnection;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use trestle_protocol::{Audience, Contract, ContractDoc};

use crate::input::decode;
use crate::{Problem, db};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Runs on a plain connection; no transaction, no recheck.
    Read,
    /// Runs in a transaction the executor opens, rechecks and commits.
    Write,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Read => "read",
            Mode::Write => "write",
        }
    }
}

/// Static facts about an operation: stable identity, not behavior.
#[derive(Debug, Clone, Copy)]
pub struct OperationInfo {
    /// Stable ID, namespaced by the owning app: `lessons.create`.
    pub id: &'static str,
    pub version: &'static str,
    pub mode: Mode,
    /// Publication profiles this operation appears in (OpenAPI, forms,
    /// admin). Never authority: the policy still decides.
    pub audiences: &'static [Audience],
    pub summary: &'static str,
}

/// Authorization bound to an operation. Deny by default; policies decide
/// which problem a denial produces (403, or 404 to conceal existence).
pub trait Policy<C>: Send + Sync + 'static {
    /// Stable policy ID, shown by inspection.
    fn id(&self) -> &'static str;

    /// Operation-level check against the admitted context, before any
    /// transaction.
    fn admit(&self, ctx: &C) -> Result<(), Problem>;

    /// Re-evaluated inside the write transaction, before the operation runs,
    /// against current state (membership may have changed since admission).
    fn recheck(
        &self,
        conn: &mut PgConnection,
        ctx: &C,
    ) -> impl Future<Output = Result<(), Problem>> + Send;
}

/// What a request context needs from the transport, for exports and
/// inspection. Extraction itself is the context's `FromRequestParts` impl.
pub trait RequestContext: Send + Sync + 'static {
    fn describe() -> ContextDescriptor;
}

#[derive(Debug, Clone, Default)]
pub struct ContextDescriptor {
    /// Requires `Authorization: Bearer …`.
    pub authenticated: bool,
    pub headers: Vec<HeaderRequirement>,
}

#[derive(Debug, Clone)]
pub struct HeaderRequirement {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema `format` of the header value, if any.
    pub format: Option<&'static str>,
}

impl ContextDescriptor {
    pub fn authenticated() -> Self {
        Self {
            authenticated: true,
            headers: Vec::new(),
        }
    }

    pub fn header(
        mut self,
        name: &'static str,
        description: &'static str,
        format: Option<&'static str>,
    ) -> Self {
        self.headers.push(HeaderRequirement {
            name,
            description,
            format,
        });
        self
    }
}

/// The behavior behind one exposed capability.
///
/// Every operation names its policy; there is no unprotected default. A
/// public operation must say so with a policy that admits everyone.
///
/// ```
/// # use std::sync::LazyLock;
/// # use serde::{Deserialize, Serialize};
/// # use sqlx::PgConnection;
/// # use trestle_core::Problem;
/// # use trestle_core::operation::*;
/// # use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
/// # struct Ctx;
/// # impl RequestContext for Ctx {
/// #     fn describe() -> ContextDescriptor { ContextDescriptor::authenticated() }
/// # }
/// # #[derive(Serialize, Deserialize)]
/// # struct Echo { text: String }
/// # impl Contract for Echo {
/// #     fn contract() -> &'static ContractDoc {
/// #         static C: LazyLock<ContractDoc> = LazyLock::new(|| {
/// #             ContractDoc::input("demo.echo", "1").field(Field::text("text"))
/// #         });
/// #         &C
/// #     }
/// # }
/// # struct Members;
/// # impl Policy<Ctx> for Members {
/// #     fn id(&self) -> &'static str { "demo.members" }
/// #     fn admit(&self, _: &Ctx) -> Result<(), Problem> { Ok(()) }
/// #     async fn recheck(&self, _: &mut PgConnection, _: &Ctx) -> Result<(), Problem> { Ok(()) }
/// # }
/// struct EchoOp;
///
/// impl Operation for EchoOp {
///     type Context = Ctx;
///     type Input = Echo;
///     type Output = Echo;
///     type Policy = Members;
///
///     const INFO: OperationInfo = OperationInfo {
///         id: "demo.echo",
///         version: "1",
///         mode: Mode::Read,
///         audiences: &[Audience::Product],
///         summary: "Echo the input.",
///     };
///
///     fn policy(&self) -> &Members {
///         &Members
///     }
///
///     async fn run(&self, _: &mut PgConnection, _: &Ctx, input: Echo) -> Result<Echo, Problem> {
///         Ok(input)
///     }
/// }
/// ```
///
/// Leaving the policy out is a compile error, not a runtime check (E0046,
/// missing trait items):
///
/// ```compile_fail,E0046
/// # use std::sync::LazyLock;
/// # use serde::{Deserialize, Serialize};
/// # use sqlx::PgConnection;
/// # use trestle_core::Problem;
/// # use trestle_core::operation::*;
/// # use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
/// # struct Ctx;
/// # impl RequestContext for Ctx {
/// #     fn describe() -> ContextDescriptor { ContextDescriptor::authenticated() }
/// # }
/// # #[derive(Serialize, Deserialize)]
/// # struct Echo { text: String }
/// # impl Contract for Echo {
/// #     fn contract() -> &'static ContractDoc {
/// #         static C: LazyLock<ContractDoc> = LazyLock::new(|| {
/// #             ContractDoc::input("demo.echo", "1").field(Field::text("text"))
/// #         });
/// #         &C
/// #     }
/// # }
/// # struct Members;
/// # impl Policy<Ctx> for Members {
/// #     fn id(&self) -> &'static str { "demo.members" }
/// #     fn admit(&self, _: &Ctx) -> Result<(), Problem> { Ok(()) }
/// #     async fn recheck(&self, _: &mut PgConnection, _: &Ctx) -> Result<(), Problem> { Ok(()) }
/// # }
/// struct EchoOp;
///
/// impl Operation for EchoOp {
///     type Context = Ctx;
///     type Input = Echo;
///     type Output = Echo;
///
///     const INFO: OperationInfo = OperationInfo {
///         id: "demo.echo",
///         version: "1",
///         mode: Mode::Read,
///         audiences: &[Audience::Product],
///         summary: "Echo the input.",
///     };
///
///     async fn run(&self, _: &mut PgConnection, _: &Ctx, input: Echo) -> Result<Echo, Problem> {
///         Ok(input)
///     }
/// }
/// ```
pub trait Operation: Send + Sync + 'static {
    type Context: RequestContext;
    type Input: Contract + Serialize + DeserializeOwned + Send + Sync;
    type Output: Contract + Serialize + Send;
    type Policy: Policy<Self::Context>;

    const INFO: OperationInfo;

    fn policy(&self) -> &Self::Policy;

    /// The behavior. For `Mode::Write`, `conn` is the executor's transaction;
    /// anything written here commits or rolls back together.
    fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &Self::Context,
        input: Self::Input,
    ) -> impl Future<Output = Result<Self::Output, Problem>> + Send;
}

/// Executes with typed input. The input still passes through its contract,
/// so Rust callers cannot bypass validation that HTTP callers get.
pub async fn execute<O: Operation>(
    pool: &PgPool,
    op: &O,
    ctx: &O::Context,
    input: O::Input,
) -> Result<O::Output, Problem> {
    let value = serde_json::to_value(&input).map_err(|err| {
        tracing::error!(operation = O::INFO.id, error = %err, "input does not serialize");
        Problem::internal()
    })?;
    execute_json(pool, op, ctx, &value).await
}

/// Executes with an untrusted JSON instance, e.g. a request body.
pub async fn execute_json<O: Operation>(
    pool: &PgPool,
    op: &O,
    ctx: &O::Context,
    input: &Value,
) -> Result<O::Output, Problem> {
    let input = validate::<O::Input>(input)?;
    execute_validated(pool, op, ctx, input).await
}

fn observe_pool_wait(started: std::time::Instant) {
    crate::metrics::observe(
        "trestle_db_acquire_seconds",
        "Time waiting for a pooled connection (and BEGIN, for writes).",
        &[],
        started.elapsed().as_secs_f64(),
    );
}

/// Input that has passed its contract. Only [`validate`] constructs it, so
/// holding one proves validation ran.
pub struct Validated<T>(T);

pub fn validate<T: Contract + DeserializeOwned>(input: &Value) -> Result<Validated<T>, Problem> {
    decode(input).map(Validated)
}

/// Everything after decoding: authorization, transaction, run, commit.
pub async fn execute_validated<O: Operation>(
    pool: &PgPool,
    op: &O,
    ctx: &O::Context,
    Validated(input): Validated<O::Input>,
) -> Result<O::Output, Problem> {
    let info = O::INFO;
    let started = std::time::Instant::now();
    let result = async {
        op.policy().admit(ctx)?;
        match info.mode {
            Mode::Read => {
                let waiting = std::time::Instant::now();
                let mut conn = pool.acquire().await?;
                observe_pool_wait(waiting);
                op.run(&mut conn, ctx, input).await
            }
            Mode::Write => {
                let waiting = std::time::Instant::now();
                let mut tx = pool.begin().await?;
                observe_pool_wait(waiting);
                op.policy().recheck(&mut tx, ctx).await?;
                let output = op.run(&mut tx, ctx, input).await?;
                db::commit(tx).await?;
                Ok(output)
            }
        }
    }
    .await;
    let outcome = match &result {
        Ok(_) => "ok",
        Err(problem) => problem.code(),
    };
    crate::metrics::increment(
        "trestle_operation_total",
        "Operation executions by outcome (ok or problem code).",
        &[("operation", info.id), ("outcome", outcome)],
    );
    crate::metrics::observe(
        "trestle_operation_duration_seconds",
        "Operation latency, including authorization and commit.",
        &[("operation", info.id)],
        started.elapsed().as_secs_f64(),
    );
    result
}

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The request context of a [`DynOperation`], whatever its type.
pub type DynContext = Arc<dyn Any + Send + Sync>;

/// The connection a [`DynOperation`] runs on, handed over by value so the
/// operation can share it with code that outlives a borrow, and handed back
/// so the executor can commit.
pub enum DynConn {
    /// `Mode::Read`: a pooled connection, no transaction.
    Read(PoolConnection<Postgres>),
    /// `Mode::Write`: the executor's transaction.
    Write(Transaction<'static, Postgres>),
}

impl DynConn {
    pub fn conn(&mut self) -> &mut PgConnection {
        match self {
            DynConn::Read(conn) => conn,
            DynConn::Write(tx) => tx,
        }
    }
}

/// An operation assembled at runtime (for example from an intent file)
/// rather than written as a Rust type. It has the same parts as
/// [`Operation`] (contracts, a policy, a mode) and goes through the same
/// executor sequence ([`execute_dyn`]); inputs and outputs are JSON.
pub trait DynOperation<S>: Send + Sync + 'static {
    fn info(&self) -> OperationInfo;
    fn policy_id(&self) -> &'static str;
    fn context(&self) -> ContextDescriptor;
    fn input(&self) -> &'static ContractDoc;
    fn output(&self) -> &'static ContractDoc;

    /// Identity and tenant from the request, before anything else is read.
    fn context_from<'a>(
        &'a self,
        parts: &'a mut Parts,
        state: &'a S,
    ) -> BoxFuture<'a, Result<DynContext, Problem>>;

    /// Operation-level authorization, before any transaction.
    fn admit(&self, ctx: &DynContext) -> Result<(), Problem>;

    /// Re-checked inside the write transaction, before `run`.
    fn recheck<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        ctx: &'a DynContext,
    ) -> BoxFuture<'a, Result<(), Problem>>;

    /// The behavior. Must return the connection it was given.
    fn run(
        self: Arc<Self>,
        conn: DynConn,
        ctx: DynContext,
        input: Value,
    ) -> BoxFuture<'static, (DynConn, Result<Value, Problem>)>;
}

/// The executor for [`DynOperation`]s: validate against the input contract,
/// admit, then read on a connection or write in a transaction (recheck, run,
/// commit). The same sequence and metrics as [`execute_validated`].
pub async fn execute_dyn<S: 'static>(
    pool: &PgPool,
    op: Arc<dyn DynOperation<S>>,
    ctx: DynContext,
    input: &Value,
) -> Result<Value, Problem> {
    let info = op.info();
    let started = std::time::Instant::now();
    let result = async {
        let input = op.input().validate(input).map_err(Problem::from)?;
        op.admit(&ctx)?;
        match info.mode {
            Mode::Read => {
                let waiting = std::time::Instant::now();
                let conn = pool.acquire().await?;
                observe_pool_wait(waiting);
                let (_, result) = op.clone().run(DynConn::Read(conn), ctx, input).await;
                result
            }
            Mode::Write => {
                let waiting = std::time::Instant::now();
                let mut tx = pool.begin().await?;
                observe_pool_wait(waiting);
                op.recheck(&mut tx, &ctx).await?;
                let (conn, result) = op.clone().run(DynConn::Write(tx), ctx, input).await;
                let output = result?;
                match conn {
                    DynConn::Write(tx) => db::commit(tx).await?,
                    DynConn::Read(_) => {
                        tracing::error!(operation = info.id, "write ran without its transaction");
                        return Err(Problem::internal());
                    }
                }
                Ok(output)
            }
        }
    }
    .await;
    let outcome = match &result {
        Ok(_) => "ok",
        Err(problem) => problem.code(),
    };
    crate::metrics::increment(
        "trestle_operation_total",
        "Operation executions by outcome (ok or problem code).",
        &[("operation", info.id), ("outcome", outcome)],
    );
    crate::metrics::observe(
        "trestle_operation_duration_seconds",
        "Operation latency, including authorization and commit.",
        &[("operation", info.id)],
        started.elapsed().as_secs_f64(),
    );
    result
}
