//! Operations and the executor (design doc §6).
//!
//! An [`Operation`] is the one place a behavior lives. Every caller (managed
//! HTTP route, hand-written Axum handler, test, later forms/admin/jobs) goes
//! through [`execute`] or [`execute_json`], which apply the same sequence:
//!
//! ```text
//! decode + portable validation (contract)
//!   -> operation-level authorization (Policy::admit)
//!   -> Write: begin transaction -> Policy::recheck -> run -> check output -> commit (classified)
//!      Read:  acquire connection -> run -> check output
//! ```
//!
//! The executor owns the transaction. Operations receive an explicit
//! connection and never touch the pool. An output that does not satisfy its
//! contract is a server fault (500): a write rolls back instead of committing
//! (ADR-012).

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::FromRef;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::pool::PoolConnection;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use trestle_protocol::{Audience, Contract, ContractDoc};

use crate::effects::{self, Emits};
pub use crate::effects::{Effects, EffectsInfo};
use crate::input::decode;
use crate::invocation::{FromInvocation, Invocation};
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

/// Static facts about an operation: stable identity, not behavior. Typed
/// operations declare it as a `const`; the registry keeps an owned
/// [`OperationDescriptor`] built from it.
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

/// The same facts as [`OperationInfo`], owned, so operations assembled from
/// runtime data need no `'static` strings (ADR-014).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDescriptor {
    pub id: Arc<str>,
    pub version: Arc<str>,
    pub mode: Mode,
    pub audiences: Arc<[Audience]>,
    pub summary: Arc<str>,
    /// Declared effects (ADR-018); `None` declares nothing.
    pub effects: Option<Arc<Effects>>,
}

impl OperationDescriptor {
    pub fn new(id: impl Into<Arc<str>>, version: impl Into<Arc<str>>, mode: Mode) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            mode,
            audiences: Arc::new([]),
            summary: Arc::from(""),
            effects: None,
        }
    }

    pub fn effects(mut self, effects: Effects) -> Self {
        self.effects = Some(Arc::new(effects));
        self
    }

    pub fn audiences(mut self, audiences: &[Audience]) -> Self {
        self.audiences = Arc::from(audiences);
        self
    }

    pub fn summary(mut self, summary: impl Into<Arc<str>>) -> Self {
        self.summary = summary.into();
        self
    }
}

/// The running write's declared emits, borrowed from its `const`.
fn typed_emits<O: Operation>() -> Option<Emits> {
    O::EFFECTS.map(|e| Emits::Static(O::INFO.id, e.emits))
}

fn dyn_emits(info: &OperationDescriptor) -> Option<Emits> {
    info.effects
        .clone()
        .map(|e| Emits::Owned(info.id.clone(), e))
}

impl From<OperationInfo> for OperationDescriptor {
    fn from(info: OperationInfo) -> Self {
        Self::new(info.id, info.version, info.mode)
            .audiences(info.audiences)
            .summary(info.summary)
    }
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

/// What a context needs from the transport. Exports and inspection publish
/// it, and HTTP reads exactly this much into an [`Invocation`] (ADR-013).
/// The context itself is built by its [`FromInvocation`] impl.
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

    /// What it reads, writes, enqueues and calls out to (ADR-018). `None`
    /// declares nothing and is unchecked; a declared write may enqueue only
    /// what it lists in `emits`.
    const EFFECTS: Option<EffectsInfo> = None;

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

/// Checks an operation's result against its output contract and returns the
/// normalized instance. A mismatch is the operation's bug, not the caller's:
/// it is logged with field pointers and codes (never values) and becomes a
/// plain 500.
fn check_output(operation: &str, contract: &ContractDoc, output: &Value) -> Result<Value, Problem> {
    contract.validate(output).map_err(|rejection| {
        let errors: Vec<String> = match rejection.errors() {
            [] => vec!["(not an object)".to_owned()],
            errors => errors
                .iter()
                .map(|e| format!("{} {}", e.pointer, e.code))
                .collect(),
        };
        tracing::error!(
            operation,
            contract = %contract.id,
            errors = ?errors,
            "output does not satisfy its contract"
        );
        crate::metrics::increment(
            "trestle_operation_output_invalid_total",
            "Operation results refused because they do not satisfy the output contract.",
            &[("operation", operation)],
        );
        Problem::internal()
    })
}

fn check_typed_output<O: Operation>(output: &O::Output) -> Result<(), Problem> {
    let value = serde_json::to_value(output).map_err(|err| {
        tracing::error!(operation = O::INFO.id, error = %err, "output does not serialize");
        Problem::internal()
    })?;
    check_output(O::INFO.id, O::Output::contract(), &value).map(drop)
}

fn check_replayed_output(
    operation: &str,
    contract: &ContractDoc,
    body: &str,
) -> Result<(), Problem> {
    // A retained receipt can outlive an output projection. Never disclose
    // fields rejected by today's contract, and never re-enter execution when
    // that stored response can no longer be returned.
    let output: Value = serde_json::from_str(body).map_err(|_| Problem::internal())?;
    check_output(operation, contract, &output).map(drop)
}

/// Everything after decoding: authorization, transaction, run, output
/// check, commit.
pub async fn execute_validated<O: Operation>(
    pool: &PgPool,
    op: &O,
    ctx: &O::Context,
    Validated(input): Validated<O::Input>,
) -> Result<O::Output, Problem> {
    let info = O::INFO;
    let started = std::time::Instant::now();
    let result: Result<O::Output, Problem> = async {
        op.policy().admit(ctx)?;
        match info.mode {
            Mode::Read => {
                let waiting = std::time::Instant::now();
                let output = if db::read_only_reads() {
                    let mut tx = db::begin_read(pool).await?;
                    observe_pool_wait(waiting);
                    // Dropping `tx` queues its ROLLBACK, which the pool
                    // sends with its usual check when the connection
                    // comes back.
                    op.run(&mut tx, ctx, input).await?
                } else {
                    let mut conn = pool.acquire().await?;
                    observe_pool_wait(waiting);
                    op.run(&mut conn, ctx, input).await?
                };
                check_typed_output::<O>(&output)?;
                Ok(output)
            }
            Mode::Write => {
                let waiting = std::time::Instant::now();
                let mut tx = crate::fence::begin(pool).await?;
                observe_pool_wait(waiting);
                op.policy().recheck(&mut tx, ctx).await?;
                let output =
                    effects::scoped(typed_emits::<O>(), op.run(&mut tx, ctx, input)).await?;
                // Returning early drops `tx`, which rolls it back.
                check_typed_output::<O>(&output)?;
                db::commit(tx).await?;
                Ok(output)
            }
        }
    }
    .await;
    observe_execution(info.id, started, result.as_ref().err());
    result
}

fn observe_execution(operation: &str, started: std::time::Instant, problem: Option<&Problem>) {
    let outcome = problem.map_or("ok", Problem::code);
    crate::metrics::increment(
        "trestle_operation_total",
        "Operation executions by outcome (ok or problem code).",
        &[("operation", operation), ("outcome", outcome)],
    );
    crate::metrics::observe(
        "trestle_operation_duration_seconds",
        "Operation latency, including authorization and commit.",
        &[("operation", operation)],
        started.elapsed().as_secs_f64(),
    );
}

/// The answer to a keyed write (ADR-016): the committed response body as
/// JSON text, and whether it was replayed from an earlier attempt's receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyed {
    pub body: String,
    pub replayed: bool,
}

fn count_replay(operation: &str) {
    crate::metrics::increment(
        "trestle_idempotency_replays_total",
        "Keyed writes answered from a committed receipt.",
        &[("operation", operation)],
    );
}

/// [`execute_validated`] for a write carrying `key` (ADR-016): the receipt
/// is claimed after `recheck` in the same transaction, and either the
/// committed answer is replayed or the operation runs and its answer is
/// stored before commit.
pub async fn execute_keyed<O: Operation>(
    pool: &PgPool,
    op: &O,
    ctx: &O::Context,
    Validated(input): Validated<O::Input>,
    invocation: &Invocation,
    key: &str,
) -> Result<Keyed, Problem> {
    let info = O::INFO;
    let started = std::time::Instant::now();
    let result: Result<Keyed, Problem> = async {
        if info.mode != Mode::Write {
            return Err(crate::idempotency::unsupported());
        }
        let canonical = serde_json::to_value(&input).map_err(|err| {
            tracing::error!(operation = info.id, error = %err, "input does not serialize");
            Problem::internal()
        })?;
        let receipt =
            crate::idempotency::Receipt::new(invocation, info.id, info.version, key, &canonical);
        op.policy().admit(ctx)?;
        let waiting = std::time::Instant::now();
        let mut tx = crate::fence::begin(pool).await?;
        observe_pool_wait(waiting);
        op.policy().recheck(&mut tx, ctx).await?;
        match crate::idempotency::claim(&mut tx, &receipt).await? {
            crate::idempotency::Claim::Replay(body) => {
                check_replayed_output(info.id, O::Output::contract(), &body)?;
                count_replay(info.id);
                Ok(Keyed {
                    body,
                    replayed: true,
                })
            }
            crate::idempotency::Claim::Fresh => {
                let output =
                    effects::scoped(typed_emits::<O>(), op.run(&mut tx, ctx, input)).await?;
                check_typed_output::<O>(&output)?;
                let body = serde_json::to_string(&output).map_err(|err| {
                    tracing::error!(operation = info.id, error = %err, "output does not serialize");
                    Problem::internal()
                })?;
                crate::idempotency::complete(&mut tx, &receipt, &body).await?;
                db::commit(tx).await?;
                Ok(Keyed {
                    body,
                    replayed: false,
                })
            }
        }
    }
    .await;
    observe_execution(info.id, started, result.as_ref().err());
    result
}

/// [`execute_keyed`] for a [`DynOperation`].
pub async fn execute_dyn_keyed<S: 'static>(
    pool: &PgPool,
    op: Arc<dyn DynOperation<S>>,
    ctx: DynContext,
    input: &Value,
    invocation: &Invocation,
    key: &str,
) -> Result<Keyed, Problem> {
    let info = op.info();
    let started = std::time::Instant::now();
    let result: Result<Keyed, Problem> = async {
        if info.mode != Mode::Write {
            return Err(crate::idempotency::unsupported());
        }
        let input = op.input().validate(input).map_err(Problem::from)?;
        let receipt =
            crate::idempotency::Receipt::new(invocation, &info.id, &info.version, key, &input);
        op.admit(&ctx)?;
        let waiting = std::time::Instant::now();
        let mut tx = crate::fence::begin(pool).await?;
        observe_pool_wait(waiting);
        op.recheck(&mut tx, &ctx).await?;
        if let crate::idempotency::Claim::Replay(body) =
            crate::idempotency::claim(&mut tx, &receipt).await?
        {
            check_replayed_output(&info.id, op.output(), &body)?;
            let output: Value = serde_json::from_str(&body).map_err(|_| Problem::internal())?;
            op.recheck_replay(&mut tx, &ctx, &output).await?;
            count_replay(&info.id);
            return Ok(Keyed {
                body,
                replayed: true,
            });
        }
        let (conn, result) = effects::scoped(
            dyn_emits(info),
            op.clone().run(DynConn::Write(tx), ctx, input),
        )
        .await;
        let output = result?;
        let DynConn::Write(mut tx) = conn else {
            tracing::error!(operation = &*info.id, "write ran without its transaction");
            return Err(Problem::internal());
        };
        let output = check_output(&info.id, op.output(), &output)?;
        let body = output.to_string();
        crate::idempotency::complete(&mut tx, &receipt, &body).await?;
        db::commit(tx).await?;
        Ok(Keyed {
            body,
            replayed: false,
        })
    }
    .await;
    observe_execution(&info.id, started, result.as_ref().err());
    result
}

/// Runs `op` for a caller that is not an HTTP request (a job, an operator
/// command, a test): resolves its context from `invocation`, then validates
/// and executes exactly as a managed route does (ADR-013).
pub async fn invoke<S, O>(
    state: &S,
    op: &O,
    invocation: &Invocation,
    input: &Value,
) -> Result<O::Output, Problem>
where
    O: Operation,
    O::Context: FromInvocation<S>,
    PgPool: FromRef<S>,
{
    if invocation.idempotency_key().is_some() {
        // Only the registry knows whether an operation accepts a key.
        return Err(crate::idempotency::unsupported());
    }
    let ctx = O::Context::resolve(invocation, state).await?;
    invocation.check_operation_version(O::INFO.version)?;
    execute_json(&PgPool::from_ref(state), op, &ctx, input).await
}

/// [`invoke`] for a [`DynOperation`].
pub async fn invoke_dyn<S: 'static>(
    state: &S,
    op: Arc<dyn DynOperation<S>>,
    invocation: &Invocation,
    input: &Value,
) -> Result<Value, Problem>
where
    PgPool: FromRef<S>,
{
    if invocation.idempotency_key().is_some() {
        return Err(crate::idempotency::unsupported());
    }
    let ctx = op.resolve_context(invocation, state).await?;
    invocation.check_operation_version(&op.info().version)?;
    execute_dyn(&PgPool::from_ref(state), op, ctx, input).await
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
    /// `Mode::Read` with read-only reads on ([`db::read_only_reads`]): a
    /// read-only transaction, so writes fail.
    ReadOnly(Transaction<'static, Postgres>),
    /// `Mode::Write`: the executor's transaction.
    Write(Transaction<'static, Postgres>),
}

impl DynConn {
    pub fn conn(&mut self) -> &mut PgConnection {
        match self {
            DynConn::Read(conn) => conn,
            DynConn::ReadOnly(tx) | DynConn::Write(tx) => tx,
        }
    }
}

/// An operation assembled at runtime (for example from an intent file)
/// rather than written as a Rust type. It has the same parts as
/// [`Operation`] (contracts, a policy, a mode) and goes through the same
/// executor sequence ([`execute_dyn`]); inputs and outputs are JSON.
pub trait DynOperation<S>: Send + Sync + 'static {
    /// Identity, owned by the operation (ADR-014): read on every call, so
    /// borrowed rather than built.
    fn info(&self) -> &OperationDescriptor;
    fn policy_id(&self) -> &str;
    fn context(&self) -> ContextDescriptor;
    fn input(&self) -> &ContractDoc;
    fn output(&self) -> &ContractDoc;

    /// The context for this caller, from what its transport established
    /// (ADR-013): the same lookup for HTTP, jobs, commands and tests.
    fn resolve_context<'a>(
        &'a self,
        invocation: &'a Invocation,
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

    /// Re-authorizes a stored answer before an `Idempotency-Key` replay
    /// returns it (ADR-016), inside the write transaction and after
    /// [`DynOperation::recheck`]. An operation whose authority depends on
    /// the record, not only on the caller (row scopes, ADR-040), refuses
    /// here what a fresh request would refuse, so a replay never returns a
    /// record the caller can no longer reach. The default admits.
    fn recheck_replay<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        ctx: &'a DynContext,
        output: &'a Value,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        let _ = (conn, ctx, output);
        Box::pin(async { Ok(()) })
    }
}

/// A policy assembled at runtime: a [`DynOperation`]'s context lookup and
/// admission, without an operation. An admin profile built from data uses
/// one ([`crate::registry::ResourceRegistration::admin_dyn`]).
pub trait DynPolicy<S>: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn context(&self) -> ContextDescriptor;

    /// The context for this caller, as [`DynOperation::resolve_context`].
    fn resolve_context<'a>(
        &'a self,
        invocation: &'a Invocation,
        state: &'a S,
    ) -> BoxFuture<'a, Result<DynContext, Problem>>;

    fn admit(&self, ctx: &DynContext) -> Result<(), Problem>;
}

/// The executor for [`DynOperation`]s: validate against the input contract,
/// admit, then read on a connection or write in a transaction (recheck, run,
/// output check, commit). The same sequence and metrics as
/// [`execute_validated`]; the returned value is the normalized output.
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
                let conn = if db::read_only_reads() {
                    DynConn::ReadOnly(db::begin_read(pool).await?)
                } else {
                    DynConn::Read(pool.acquire().await?)
                };
                observe_pool_wait(waiting);
                let (_, result) = op.clone().run(conn, ctx, input).await;
                check_output(&info.id, op.output(), &result?)
            }
            Mode::Write => {
                let waiting = std::time::Instant::now();
                let mut tx = crate::fence::begin(pool).await?;
                observe_pool_wait(waiting);
                op.recheck(&mut tx, &ctx).await?;
                let (conn, result) = effects::scoped(
                    dyn_emits(info),
                    op.clone().run(DynConn::Write(tx), ctx, input),
                )
                .await;
                let output = result?;
                match conn {
                    DynConn::Write(tx) => {
                        // Returning early drops `tx`, which rolls it back.
                        let output = check_output(&info.id, op.output(), &output)?;
                        db::commit(tx).await?;
                        Ok(output)
                    }
                    DynConn::Read(_) | DynConn::ReadOnly(_) => {
                        tracing::error!(operation = &*info.id, "write ran without its transaction");
                        Err(Problem::internal())
                    }
                }
            }
        }
    }
    .await;
    observe_execution(&info.id, started, result.as_ref().err());
    result
}
