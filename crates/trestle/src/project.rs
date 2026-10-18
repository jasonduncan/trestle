//! The project builder and its command line (the `manage.py` pattern,
//! design §3.1): the project binary *is* the CLI.
//!
//! `check`, `inspect`, `contracts`, `interactions`, `openapi`, `sdk`,
//! `makemigrations` and `schema show` only assemble the registry: they never
//! touch the database or network.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{ArgMatches, Args, Command, FromArgMatches, Subcommand, ValueEnum};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use trestle_core::identity::ActorId;
use trestle_core::protocol::{Audience, to_pretty};
use trestle_core::registry::{App as CoreApp, Project as CoreProject, Registry, RouteKind};
use trestle_historical::artifact;
use trestle_historical::planner::{self, Plan, PlanOptions};
use trestle_historical::{HistoryError, MigrationSet};
use trestle_identity::password;
use trestle_mail::Mailer;
use uuid::Uuid;

use crate::intent::{Handlers, Resolved};
use crate::mail::MailArgs;
use crate::migrations::{self as source, Source};
use crate::reconcile::{self, Failure, exit};
use crate::serve::{self, ServeConfig};
use crate::{App, Environment, Erased, State, StateParts};

type BoxFuture = Pin<Box<dyn Future<Output = Result<(), String>>>>;
type Handler<S, C> = Box<dyn Fn(C, CommandContext<S>) -> BoxFuture>;

/// Project-specific `serve` settings, and how the project's state is built
/// from what `serve` set up. The default [`State`] needs none:
/// [`NoExtension`].
pub trait ServeExtension<S>: Args + Clone + Send + Sync + 'static {
    /// Everything wrong with these settings; reported with the built-in
    /// checks, all at once, before anything starts.
    fn problems(&self) -> Vec<String> {
        Vec::new()
    }

    fn state(self, parts: StateParts) -> Result<S, String>;
}

/// No extra `serve` settings; builds the default [`State`].
#[derive(Args, Clone, Debug, Default)]
pub struct NoExtension {}

impl ServeExtension<State> for NoExtension {
    fn state(self, parts: StateParts) -> Result<State, String> {
        Ok(parts.into())
    }
}

/// No project-specific commands.
#[derive(Clone, Copy, Debug)]
pub enum NoCommands {}

impl FromArgMatches for NoCommands {
    fn from_arg_matches(_: &ArgMatches) -> Result<Self, clap::Error> {
        Err(clap::Error::new(clap::error::ErrorKind::InvalidSubcommand))
    }
    fn update_from_arg_matches(&mut self, _: &ArgMatches) -> Result<(), clap::Error> {
        match *self {}
    }
}

impl Subcommand for NoCommands {
    fn augment_subcommands(cmd: Command) -> Command {
        cmd
    }
    fn augment_subcommands_for_update(cmd: Command) -> Command {
        cmd
    }
    fn has_subcommand(_: &str) -> bool {
        false
    }
}

/// What a project command gets: the assembled registry and the global
/// settings. Never print [`CommandContext::database`]; it holds credentials.
pub struct CommandContext<S> {
    pub registry: Arc<Registry<S>>,
    pub env: Environment,
    /// The project's display name (`Project::named`).
    pub name: Arc<str>,
    /// How to refer to this binary in messages (`notes`, `myapp`, …).
    pub bin: String,
    database_url: Option<String>,
}

impl<S> Clone for CommandContext<S> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            env: self.env,
            name: self.name.clone(),
            bin: self.bin.clone(),
            database_url: self.database_url.clone(),
        }
    }
}

impl<S> CommandContext<S> {
    /// Connection options from `--database-url` / `DATABASE_URL`.
    pub fn database(&self) -> Result<PgConnectOptions, String> {
        self.database_url
            .as_deref()
            .ok_or("DATABASE_URL is not set")?
            .parse()
            .map_err(|e| format!("invalid DATABASE_URL: {e}"))
    }

    /// A small pool for operator commands.
    pub async fn connect(&self) -> Result<PgPool, String> {
        connect(self.database()?).await
    }
}

/// A project on the default [`State`]. Install apps, then `.run()`.
pub fn project() -> Project {
    Project::with_state()
}

/// The composition root of a project and its command line.
pub struct Project<S = State, X = NoExtension, C = NoCommands> {
    name: Arc<str>,
    named: bool,
    version: Arc<str>,
    apps: Vec<Box<dyn CoreApp<S>>>,
    source: Source,
    migrations_dir: Option<PathBuf>,
    foreign: Vec<String>,
    excluded: BTreeSet<String>,
    commands: Option<Handler<S, C>>,
    intent: IntentSource,
    handlers: Handlers,
    _extension: std::marker::PhantomData<fn() -> X>,
}

impl Project<State, NoExtension, NoCommands> {
    /// Installs an app on the default state.
    pub fn install(self, app: impl App) -> Self {
        self.install_app(Erased(app))
    }
}

impl<S: Clone + Send + Sync + 'static> Project<S, NoExtension, NoCommands> {
    /// A project with its own state type `S`. Use [`Project::serve_with`]
    /// to say how `serve` builds it.
    pub fn with_state() -> Self {
        let name = std::env::var("CARGO_PKG_NAME")
            .ok()
            .or_else(bin_name)
            .unwrap_or_else(|| "trestle project".to_owned());
        Self {
            name: Arc::from(name),
            named: false,
            version: Arc::from("0.0.0"),
            apps: Vec::new(),
            source: Source::Dir,
            migrations_dir: None,
            foreign: Vec::new(),
            excluded: BTreeSet::new(),
            commands: None,
            intent: IntentSource::Default,
            handlers: Handlers::default(),
            _extension: std::marker::PhantomData,
        }
    }
}

impl<S: 'static, X, C> Project<S, X, C> {
    /// Display name and version (shown by `check`, OpenAPI, emails).
    /// Usually `.named("My API", env!("CARGO_PKG_VERSION"))`.
    pub fn named(mut self, name: impl Into<Arc<str>>, version: impl Into<Arc<str>>) -> Self {
        self.name = name.into();
        self.named = true;
        self.version = version.into();
        self
    }

    /// Installs an app written against `AppOf<S>`.
    pub fn install_app(mut self, app: impl CoreApp<S>) -> Self {
        self.apps.push(Box::new(app));
        self
    }

    /// Supplies the whole migration history from code instead of loading
    /// artifacts. An escape hatch: projects normally use the directory or
    /// [`Project::embedded_migrations`].
    pub fn migrations(
        mut self,
        history: impl Fn() -> Result<MigrationSet, HistoryError> + 'static,
    ) -> Self {
        self.source = Source::Function(Box::new(history));
        self
    }

    /// Uses artifacts compiled into the binary
    /// (`.embedded_migrations(trestle::embed_migrations!())`).
    /// `TRESTLE_MIGRATIONS_DIR`, when set, still wins.
    pub fn embedded_migrations(
        mut self,
        files: Vec<trestle_historical::load::ArtifactFile>,
    ) -> Self {
        self.source = Source::Embedded(files);
        self
    }

    /// Apps whose migrations live in another project that shares this
    /// database (or that this project handed over to one). Their ledger
    /// rows are left alone instead of failing `migrate` as "applied by a
    /// newer release"; `verify` lists them under `foreign`.
    pub fn foreign_apps<I, A>(mut self, apps: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<String>,
    {
        self.foreign.extend(apps.into_iter().map(Into::into));
        self
    }

    /// The migrations directory: read at runtime, and where
    /// `makemigrations` writes. Default `./migrations`;
    /// `TRESTLE_MIGRATIONS_DIR` overrides both.
    pub fn migrations_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.migrations_dir = Some(dir.into());
        self
    }

    /// Reads the intent from this file instead of `$TRESTLE_INTENT` or
    /// `./trestle.toml`.
    pub fn intent_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.intent = IntentSource::File(path.into());
        self
    }

    /// Uses this intent text (named `name` in messages), e.g. for tests or
    /// `include_str!("../trestle.toml")`.
    pub fn intent_str(mut self, name: &str, text: &str) -> Self {
        self.intent = IntentSource::Text(name.to_owned(), text.to_owned());
        self
    }

    /// Uses this intent document (named `name` in messages), for code that
    /// builds the intent instead of reading a file (ADR-030). It is
    /// validated and served exactly like its [`IntentDoc::to_toml`] text.
    ///
    /// [`IntentDoc::to_toml`]: crate::intent::IntentDoc::to_toml
    pub fn intent_doc(mut self, name: &str, doc: crate::intent::IntentDoc) -> Self {
        self.intent = IntentSource::Doc(name.to_owned(), Box::new(doc));
        self
    }

    /// Ignores any intent file: a project made only of Rust apps.
    pub fn without_intent(mut self) -> Self {
        self.intent = IntentSource::None;
        self
    }

    /// Registers the Rust handler an intent endpoint names
    /// (`handler = "notes::pin"`).
    pub fn handler<F, Fut>(mut self, name: &str, f: F) -> Self
    where
        F: Fn(crate::intent::Ctx, crate::intent::Input) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<crate::intent::Row, crate::Problem>> + Send + 'static,
    {
        self.handlers.handler(name, f);
        self
    }

    /// Registers a hook an intent endpoint names
    /// (`hooks = { validate = "notes::check" }`).
    pub fn hook<F, Fut>(mut self, name: &str, f: F) -> Self
    where
        F: Fn(crate::intent::Ctx, crate::intent::Row) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<crate::intent::Row, crate::Problem>> + Send + 'static,
    {
        self.handlers.hook(name, f);
        self
    }

    /// Settings `serve` adds, and how it builds the state (see
    /// [`ServeExtension`]).
    pub fn serve_with<Y: ServeExtension<S>>(self) -> Project<S, Y, C> {
        Project {
            name: self.name,
            named: self.named,
            version: self.version,
            apps: self.apps,
            source: self.source,
            foreign: self.foreign,
            migrations_dir: self.migrations_dir,
            excluded: self.excluded,
            commands: self.commands,
            intent: self.intent,
            handlers: self.handlers,
            _extension: std::marker::PhantomData,
        }
    }

    /// Project-specific commands (a clap `Subcommand` enum) and their
    /// handler. A command with the same name as a built-in one replaces it.
    pub fn commands<D, F, Fut>(self, handler: F) -> Project<S, X, D>
    where
        D: Subcommand + 'static,
        F: Fn(D, CommandContext<S>) -> Fut + 'static,
        Fut: Future<Output = Result<(), String>> + 'static,
    {
        let probe = D::augment_subcommands(Command::new("probe"));
        let excluded = probe
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        Project {
            name: self.name,
            named: self.named,
            version: self.version,
            apps: self.apps,
            source: self.source,
            foreign: self.foreign,
            migrations_dir: self.migrations_dir,
            excluded,
            commands: Some(Box::new(move |c, ctx| Box::pin(handler(c, ctx)))),
            intent: self.intent,
            handlers: self.handlers,
            _extension: std::marker::PhantomData,
        }
    }
}

impl<S, X, C> Project<S, X, C>
where
    S: Clone + Send + Sync + 'static,
    PgPool: axum::extract::FromRef<S>,
    trestle_core::identity::Identity: axum::extract::FromRef<S>,
    X: ServeExtension<S>,
    C: Subcommand + 'static,
{
    /// Assembles the registry. Pure and offline: no database or network.
    pub fn assemble(self) -> Result<Registry<S>, crate::AssemblyError> {
        let (registry, _) = self.split();
        registry
    }

    /// The registry, the committed migration history and the tenancy
    /// declared by the intent file. For [`crate::testing`].
    #[cfg(feature = "testing")]
    pub(crate) fn prepare(
        self,
    ) -> Result<
        (
            Registry<S>,
            MigrationSet,
            Option<crate::intent::spec::Tenancy>,
        ),
        String,
    > {
        let (registry, runtime) = self.split();
        let registry = registry.map_err(|e| e.to_string())?;
        let history = runtime
            .source
            .resolve(&runtime.migrations_dir, registry.data_steps())?;
        let tenancy = runtime.intent.and_then(|r| r.intent.tenancy.clone());
        Ok((registry, history, tenancy))
    }

    fn split(self) -> (Result<Registry<S>, crate::AssemblyError>, Runtime<S, C>) {
        let mut name = self.name.clone();
        let mut apps: Vec<Box<dyn CoreApp<S>>> = self.apps;
        let mut intent_problems = Vec::new();
        let mut resolved = None;
        match self.intent.load() {
            Ok(None) => {
                if !self.handlers.handlers.is_empty() || !self.handlers.hooks.is_empty() {
                    intent_problems.push(
                        "handlers or hooks are registered but there is no trestle.toml to name them"
                            .to_owned(),
                    );
                }
            }
            Ok(Some((file, intent))) => match Resolved::link(intent, &file, self.handlers) {
                Ok(r) => {
                    let r = Arc::new(r);
                    if !self.named {
                        name = Arc::from(r.intent.name.as_str());
                    }
                    for app in r.apps::<S>() {
                        apps.push(Box::new(app));
                    }
                    resolved = Some(r);
                }
                Err(problems) => intent_problems.extend(problems),
            },
            Err(problems) => intent_problems.extend(problems),
        }
        let mut core = CoreProject::new(name.clone(), self.version.clone());
        for app in apps {
            core = core.install(app);
        }
        let runtime = Runtime {
            name,
            source: self.source.with_foreign(self.foreign),
            migrations_dir: source::directory(self.migrations_dir.as_ref()),
            commands: self.commands,
            intent: resolved,
        };
        let assembled = core.assemble();
        if intent_problems.is_empty() {
            return (assembled, runtime);
        }
        let mut issues = intent_problems;
        if let Err(e) = assembled {
            issues.extend(e.issues);
        }
        (Err(crate::AssemblyError { issues }), runtime)
    }

    /// The command line for this project, for help output and tests.
    pub fn command(&self) -> Command {
        cli::<X, C>(&self.excluded)
    }

    /// Parses `std::env::args`, runs the command and returns the exit code
    /// (see [`crate::exit`]): 0 success, 1 check failed or changes pending,
    /// 2 usage error, 3 needs approval, 4 failed and rolled back, 5 partial,
    /// 6 drift, 7 lock timeout, 10 database unreachable.
    pub fn run(self) -> ExitCode {
        let _ = dotenvy::dotenv();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .try_init();
        let matches = self.command().get_matches();
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("error: start runtime: {e}");
                return ExitCode::FAILURE;
            }
        };
        match runtime.block_on(self.run_matches(&matches)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(failure) => {
                eprintln!("error: {failure}");
                ExitCode::from(failure.code)
            }
        }
    }

    /// Runs already-parsed arguments (see [`Project::command`]).
    pub async fn run_matches(self, matches: &ArgMatches) -> Result<(), Failure> {
        let usage = |e: clap::Error| Failure::new(exit::USAGE, e.to_string());
        let globals = Globals::from_arg_matches(matches).map_err(usage)?;
        let (registry, runtime) = self.split();
        let registry = Arc::new(registry.map_err(|e| e.to_string())?);
        let ctx = CommandContext {
            registry,
            env: globals.env,
            name: runtime.name,
            bin: bin_name().unwrap_or_else(|| "trestle".to_owned()),
            database_url: globals.database_url,
        };
        let name = matches.subcommand_name().unwrap_or_default();
        if C::has_subcommand(name)
            && let Some(handler) = &runtime.commands
        {
            let command = C::from_arg_matches(matches).map_err(usage)?;
            return handler(command, ctx).await.map_err(Failure::from);
        }
        let command = BuiltIn::<X>::from_arg_matches(matches).map_err(usage)?;
        builtin::<S, X>(
            command,
            &ctx,
            &runtime.source,
            &runtime.migrations_dir,
            runtime.intent.as_deref(),
        )
        .await
    }
}

struct Runtime<S, C> {
    intent: Option<Arc<Resolved>>,
    name: Arc<str>,
    source: Source,
    migrations_dir: PathBuf,
    commands: Option<Handler<S, C>>,
}

fn bin_name() -> Option<String> {
    let arg = std::env::args_os().next()?;
    Some(Path::new(&arg).file_stem()?.to_string_lossy().into_owned())
}

fn cli<X: Args, C: Subcommand>(excluded: &BTreeSet<String>) -> Command {
    let builtins = BuiltIn::<X>::augment_subcommands(Command::new("builtins"));
    let mut cmd = Globals::augment_args(
        Command::new(&*Box::leak(
            bin_name()
                .unwrap_or_else(|| "trestle".to_owned())
                .into_boxed_str(),
        ))
        .about("Trestle project command line")
        .subcommand_required(true)
        .arg_required_else_help(true),
    );
    for sub in builtins.get_subcommands() {
        if !excluded.contains(sub.get_name()) {
            cmd = cmd.subcommand(sub.clone());
        }
    }
    C::augment_subcommands(cmd)
}

#[derive(Args)]
struct MigrateArgs {
    /// Show what would run without changing anything.
    #[arg(long)]
    plan: bool,
    /// Machine output on stdout (`trestle.plan/1`, or one line per event
    /// and a `trestle.migrate/1` summary).
    #[arg(long)]
    json: bool,
    /// Apply exactly the plan with this digest (from `--plan --json`).
    #[arg(long, value_name = "PLAN_DIGEST", conflicts_with_all = ["plan", "auto"])]
    approve: Option<String>,
    /// Apply only if every step is classified `auto`; exit 3 otherwise.
    #[arg(long, conflicts_with = "plan")]
    auto: bool,
    /// Snapshot or backup ID taken before this run, stored in the ledger.
    #[arg(long, value_name = "ID")]
    backup_ref: Option<String>,
    /// How long to wait for another migrate's lock (e.g. 30s); exit 7.
    #[arg(long, value_name = "DURATION")]
    lock_timeout: Option<String>,
    /// Apply although the schema differs from applied history (exit 6
    /// otherwise). Only for a known, reviewed difference.
    #[arg(long, conflicts_with = "plan")]
    allow_drift: bool,
}

#[derive(Args)]
struct Globals {
    #[arg(long, env = "DATABASE_URL", global = true, hide_env_values = true)]
    database_url: Option<String>,

    #[arg(
        long,
        env = "TRESTLE_ENV",
        value_enum,
        default_value = "development",
        global = true
    )]
    env: Environment,
}

#[derive(Subcommand)]
enum BuiltIn<X: Args> {
    /// Serve the HTTP API. Refuses to start if required migrations are
    /// missing or (in production) if the configuration is unsafe.
    Serve {
        #[command(flatten)]
        config: Box<ServeConfig>,
        #[command(flatten)]
        extension: X,
    },
    /// Identity: actor IDs for provider accounts, session revocation.
    Identity {
        #[command(subcommand)]
        action: IdentityCommand,
    },
    /// Password accounts: invites, reset links, disable, enable, list.
    Account {
        #[command(subcommand)]
        action: AccountCommand,
    },
    /// Outgoing email.
    Mail {
        #[command(subcommand)]
        action: MailCommand,
    },
    /// Apply committed migrations. In production, only plans the
    /// classifier marks `auto` apply without `--approve`.
    Migrate(MigrateArgs),
    /// Compare the ledger and the database catalog with committed history.
    /// Exit 6 on drift, partial or unknown applied migrations.
    Verify {
        /// Machine output (`trestle.verify/1`) on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Assemble and validate the registry. Offline.
    Check,
    /// Describe the assembled registry. Offline.
    Inspect {
        #[command(subcommand)]
        what: Inspect,
    },
    /// Wire contracts: Contract IR, digests and JSON Schema. Offline.
    Contracts {
        #[command(subcommand)]
        action: Export,
    },
    /// Interaction descriptors (trestle.interactions/1.0). Offline.
    Interactions {
        #[command(subcommand)]
        action: Export,
    },
    /// OpenAPI 3.1.1 description of managed routes. Offline.
    Openapi {
        #[command(subcommand)]
        action: Export,
    },
    /// Generate migrations from model changes. Offline; writes reviewable
    /// artifacts. `--check` only reports (for CI).
    Makemigrations {
        /// Exit non-zero if models and history differ; write nothing.
        #[arg(long)]
        check: bool,
        #[arg(long, default_value = "auto")]
        name: String,
        /// Approve removing this model or field ID (data is destroyed).
        #[arg(long = "allow-destructive", value_name = "ID")]
        allow_destructive: Vec<String>,
        /// This field ID was filled by a committed data migration and may
        /// become required.
        #[arg(long, value_name = "FIELD_ID")]
        filled: Vec<String>,
        /// Build indexes on existing tables CONCURRENTLY and validate new
        /// checks separately, in a second, non-atomic migration (ADR-021).
        #[arg(long, conflicts_with = "empty")]
        online: bool,
        /// Create a data-only migration in --app that runs data step --data.
        #[arg(long, requires_all = ["app", "data"])]
        empty: bool,
        /// With --empty: the data step is batched (`DataStep::batched`), so
        /// the migration commits batch by batch and resumes (ADR-020).
        #[arg(long, requires = "empty")]
        batched: bool,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        data: Option<String>,
        /// Artifact directory (defaults to the project's migrations directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Generated TypeScript client for managed operations. Offline.
    Sdk {
        #[command(subcommand)]
        action: Export,
    },
    /// Persistence state: desired (models), history (migrations), observed (database).
    Schema {
        #[command(subcommand)]
        action: SchemaCommand,
    },
    /// Tenants and memberships declared by `[tenancy]` in trestle.toml.
    Tenant {
        #[command(subcommand)]
        action: TenantCommand,
    },
}

#[derive(Subcommand)]
enum TenantCommand {
    /// Create a tenant; prints its ID.
    Create {
        #[arg(long)]
        name: String,
    },
    /// Grant or change an actor's role in a tenant.
    Grant {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        actor: Uuid,
        #[arg(long)]
        role: String,
    },
    /// Remove an actor from a tenant.
    Revoke {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        actor: Uuid,
    },
}

#[derive(Subcommand)]
enum IdentityCommand {
    /// Print the actor ID for a provider account. Offline.
    ActorId {
        /// Issuer exactly as configured (TRESTLE_OIDC_ISSUER).
        #[arg(long)]
        issuer: String,
        /// The account's `sub` claim.
        #[arg(long)]
        subject: String,
    },
    /// Revoke every live browser session of an actor.
    Revoke {
        #[arg(long)]
        actor: Uuid,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Create an account and print a one-time invite link.
    Invite {
        #[arg(long)]
        email: String,
        /// How long the link works.
        #[arg(long, default_value_t = 168, value_parser = clap::value_parser!(u32).range(1..=720))]
        valid_hours: u32,
        #[command(flatten)]
        link: LinkArgs,
    },
    /// Print a one-time password reset link. Earlier links stop working.
    ResetLink {
        #[arg(long)]
        email: String,
        #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u32).range(1..=720))]
        valid_hours: u32,
        #[command(flatten)]
        link: LinkArgs,
    },
    /// Stop an account signing in: ends its sessions and voids its links.
    Disable {
        #[arg(long)]
        email: String,
    },
    /// Allow a disabled account to sign in again.
    Enable {
        #[arg(long)]
        email: String,
    },
    /// Every account with its status.
    List,
}

#[derive(Subcommand)]
enum MailCommand {
    /// Send a test message with the configured backend.
    Test {
        #[arg(long)]
        to: String,
        #[command(flatten)]
        mail: MailArgs,
    },
}

/// Where an account link points and whether to email it.
#[derive(Args, Clone, Debug)]
pub struct LinkArgs {
    /// Browser origin for the link; defaults to the first TRESTLE_PUBLIC_ORIGIN.
    #[arg(long = "origin", env = "TRESTLE_PUBLIC_ORIGIN", value_delimiter = ',')]
    pub origin: Vec<String>,
    /// Email the link to the account instead of printing it.
    #[arg(long)]
    pub send: bool,
    #[command(flatten)]
    pub mail: MailArgs,
}

impl LinkArgs {
    pub fn origin(&self) -> Result<&str, String> {
        self.origin
            .first()
            .map(String::as_str)
            .ok_or_else(|| "set TRESTLE_PUBLIC_ORIGIN or pass --origin for the link".into())
    }
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Print the desired schema from models, or with --history the replayed history. Offline.
    Show {
        #[arg(long)]
        history: bool,
    },
    /// Compare the database catalog with replayed history.
    Verify,
}

#[derive(Subcommand)]
enum Inspect {
    /// Installed apps in lifecycle order.
    Apps,
    /// Every route: managed, unmanaged (with waiver) and system.
    Routes,
    /// One operation: contracts, policy, binding, forms, effects.
    Operation { id: String },
    /// Declared effects of every operation (ADR-018); `--model` lists only
    /// the operations that read or write that model.
    Effects {
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum Export {
    /// Print canonical JSON to stdout.
    Export {
        #[arg(long, value_enum, default_value = "product")]
        audience: AudienceArg,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AudienceArg {
    Product,
    Admin,
}

impl From<AudienceArg> for Audience {
    fn from(arg: AudienceArg) -> Self {
        match arg {
            AudienceArg::Product => Audience::Product,
            AudienceArg::Admin => Audience::Admin,
        }
    }
}

async fn builtin<S, X>(
    command: BuiltIn<X>,
    ctx: &CommandContext<S>,
    source: &Source,
    migrations_dir: &Path,
    intent: Option<&Resolved>,
) -> Result<(), Failure>
where
    S: Clone + Send + Sync + 'static,
    PgPool: axum::extract::FromRef<S>,
    X: ServeExtension<S>,
{
    let registry = &*ctx.registry;
    let database = || ctx.database().map_err(|e| Failure::new(exit::USAGE, e));
    let env = ctx.env;
    let bin = ctx.bin.as_str();
    let history = || source.resolve(migrations_dir, registry.data_steps());
    match command {
        BuiltIn::Serve { config, extension } => {
            let mut config = *config;
            if let Some(intent) = intent {
                let problems = config.apply_auth(&intent.intent.auth, |k| std::env::var(k).ok());
                if !problems.is_empty() {
                    return Err(Failure::new(
                        exit::USAGE,
                        format!("refusing to start:\n  - {}", problems.join("\n  - ")),
                    ));
                }
                intent_migrations_applied(ctx, intent, history()?).await?;
            }
            let options = database()?;
            let set = history()?;
            schema_verified(&options, set.clone(), bin).await?;
            schema_fence_installed(&options, &set, bin).await?;
            Ok(serve::serve(ctx.registry.clone(), env, options, config, extension, bin).await?)
        }
        BuiltIn::Identity { action } => match action {
            IdentityCommand::ActorId { issuer, subject } => {
                println!("{}", trestle_identity::actor_for(&issuer, &subject));
                Ok(())
            }
            IdentityCommand::Revoke { actor } => {
                let pool = ctx.connect().await?;
                let revoked = trestle_identity::session::revoke_actor(&pool, ActorId(actor))
                    .await
                    .map_err(|e| e.to_string())?;
                println!("Revoked {revoked} live session(s) of {actor}.");
                Ok(())
            }
        },
        BuiltIn::Account { action } => Ok(account(ctx, action).await?),
        BuiltIn::Mail {
            action: MailCommand::Test { to, mail },
        } => {
            let mailer = mail.mailer(env)?;
            mailer
                .send(&crate::mail::test_message(&ctx.name, &to))
                .await
                .map_err(|e| e.to_string())?;
            println!("Sent a test message to {to} via {}.", mailer.describe());
            Ok(())
        }
        BuiltIn::Migrate(args) => migrate(ctx, database()?, history()?, args).await,
        BuiltIn::Verify { json } => {
            let options = database()?;
            let mut conn = connect_one(&options).await?;
            let report = reconcile::verify(&mut conn, &history()?).await?;
            if json {
                println!("{}", serde_json::to_string(&report).expect("serializes"));
            } else {
                for p in &report.pending {
                    println!("pending: {p}");
                }
                for u in &report.unverified {
                    println!("unverified: {u}");
                }
            }
            if report.ok {
                if !json {
                    println!("Verified: ledger and schema match committed history.");
                }
                Ok(())
            } else {
                let mut problems = report.drift.clone();
                problems.extend(report.partial.iter().map(|p| format!("partial: {p}")));
                problems.extend(
                    report
                        .unknown_applied
                        .iter()
                        .map(|u| format!("applied but unknown to this build: {u}")),
                );
                problems.extend(
                    report
                        .fenced_by
                        .iter()
                        .map(|f| format!("fenced: this build may not write until it contains {f}")),
                );
                Err(Failure::new(
                    exit::DRIFT,
                    format!("verify found drift:\n  - {}", problems.join("\n  - ")),
                ))
            }
        }
        BuiltIn::Check => {
            let mut problems = source.staleness(migrations_dir);
            match history() {
                Ok(set) => {
                    problems.extend(trestle_historical::load::unused_steps(
                        &set,
                        registry.data_steps(),
                    ));
                    let (_, plan) = migration_plan(registry, set, PlanOptions::default())?;
                    if !plan.is_empty() || !plan.blocked.is_empty() {
                        problems.push(format!(
                            "models differ from committed migrations; run `{bin} makemigrations`"
                        ));
                    }
                }
                Err(e) => problems.push(e.trim_end().to_owned()),
            }
            if !problems.is_empty() {
                return Err(format!(
                    "check found {} problem(s):\n  - {}",
                    problems.len(),
                    problems.join("\n  - ")
                )
                .into());
            }
            check(registry);
            Ok(())
        }
        BuiltIn::Inspect { what } => Ok(inspect(registry, what)?),
        BuiltIn::Contracts {
            action: Export::Export { audience },
        } => {
            print!("{}", to_pretty(&registry.contracts_export(audience.into())));
            Ok(())
        }
        BuiltIn::Interactions {
            action: Export::Export { audience },
        } => {
            print!(
                "{}",
                to_pretty(&registry.interactions_export(audience.into()))
            );
            Ok(())
        }
        BuiltIn::Openapi {
            action: Export::Export { audience },
        } => {
            print!("{}", to_pretty(&registry.openapi(audience.into())));
            Ok(())
        }
        BuiltIn::Sdk {
            action: Export::Export { audience },
        } => {
            print!("{}", registry.typescript_sdk(audience.into()));
            Ok(())
        }
        BuiltIn::Makemigrations {
            check,
            name,
            allow_destructive,
            filled,
            online,
            empty,
            batched,
            app,
            data,
            dir,
        } => {
            let dir = dir.unwrap_or_else(|| migrations_dir.to_owned());
            if empty {
                let set = history()?;
                let (app, data) = (app.unwrap_or_default(), data.unwrap_or_default());
                let artifact = if batched {
                    artifact::empty_batched(&set, &app, &name, &data)
                } else {
                    artifact::empty(&set, &app, &name, &data)
                }
                .map_err(|e| e.to_string())?;
                for path in artifact::write(&dir, &[artifact]).map_err(|e| e.to_string())? {
                    println!("wrote {path}");
                }
                let constructor = if batched {
                    "DataStep::batched"
                } else {
                    "DataStep::once"
                };
                println!(
                    "Now add the data step `pub fn {data}() -> DataStep` (built with \
                     `{constructor}`) to the project's migrations."
                );
                return Ok(());
            }
            let options = PlanOptions {
                allow_destructive: allow_destructive.into_iter().collect(),
                filled: filled.into_iter().collect(),
                online,
                ..Default::default()
            };
            let (set, plan) = migration_plan(registry, history()?, options)?;
            Ok(makemigrations(set, plan, check, &name, &dir, bin)?)
        }
        BuiltIn::Tenant { action } => Ok(tenant(ctx, intent, action).await?),
        BuiltIn::Schema { action } => match action {
            SchemaCommand::Show { history: replayed } => {
                let state = if replayed {
                    history()?.history().map_err(|e| e.to_string())?
                } else {
                    registry.desired_schema().clone()
                };
                print!(
                    "{}",
                    to_pretty(&serde_json::to_value(state).map_err(|e| e.to_string())?)
                );
                Ok(())
            }
            SchemaCommand::Verify => Ok(schema_verify(ctx.database()?, history()?, bin).await?),
        },
    }
}

/// Compares the installed models (desired) with committed history. Pure
/// and offline, like `makemigrations`.
pub fn migration_plan<S>(
    registry: &Registry<S>,
    set: MigrationSet,
    mut options: PlanOptions,
) -> Result<(MigrationSet, Plan), String> {
    let history = set.history().map_err(|e| e.to_string())?;
    options.backfills.extend(registry.backfills().clone());
    let plan = planner::diff(&history, registry.desired_schema(), &options);
    Ok((set, plan))
}

fn makemigrations(
    set: MigrationSet,
    plan: Plan,
    check: bool,
    name: &str,
    dir: &Path,
    bin: &str,
) -> Result<(), String> {
    if !plan.blocked.is_empty() {
        return Err(format!(
            "cannot generate migrations:\n  - {}",
            plan.blocked.join("\n  - ")
        ));
    }
    if plan.is_empty() {
        println!("No changes: models match committed history.");
        return Ok(());
    }
    for change in &plan.changes {
        println!("{}: {} operation(s)", change.app, change.ops.len());
        for note in &change.notes {
            println!("  note: {note}");
        }
    }
    if check {
        return Err(format!(
            "models differ from committed history; run `{bin} makemigrations`"
        ));
    }
    let artifacts = artifact::generate(&set, &plan, name).map_err(|e| e.to_string())?;
    for path in artifact::write(dir, &artifacts).map_err(|e| e.to_string())? {
        println!("wrote {path}");
    }
    println!(
        "Review the SQL, rebuild, run `{bin} migrate`, and add the new migration to the app's schema requirements if code depends on it."
    );
    Ok(())
}

/// Compares the catalog with the state of the migrations the ledger says
/// are applied; pending migrations are listed, not treated as drift.
async fn schema_verify(
    options: PgConnectOptions,
    set: MigrationSet,
    bin: &str,
) -> Result<(), String> {
    let pool = connect(options).await?;
    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    let plan = trestle_historical::plan(&mut conn, &set)
        .await
        .map_err(|e| e.to_string())?;
    let applied: Vec<_> = plan
        .entries
        .iter()
        .filter(|e| e.state == trestle_historical::PlanState::Applied)
        .map(|e| e.migration)
        .collect();
    let mut partial = Vec::new();
    for pending in plan.outstanding() {
        match pending.state {
            trestle_historical::PlanState::Retry {
                previous: trestle_historical::LedgerStatus::Partial,
                step,
                ..
            }
            | trestle_historical::PlanState::Retry {
                previous: trestle_historical::LedgerStatus::Running,
                step: step @ 1..,
                ..
            } => partial.push(format!(
                "{} is partially applied (stopped at step {}); rerun `{bin} migrate` to resume",
                pending.migration,
                step + 1
            )),
            _ => println!("pending: {} (run `{bin} migrate`)", pending.migration),
        }
    }
    let history = set
        .history_of(|key| applied.contains(&key))
        .map_err(|e| e.to_string())?;
    let report = trestle_historical::verify::verify(&mut conn, &history)
        .await
        .map_err(|e| e.to_string())?;
    for note in &report.unverified {
        println!("unverified: {note}");
    }
    for table in &report.unmanaged_tables {
        println!("unmanaged table: {table}");
    }
    if !partial.is_empty() {
        let mut problems = partial;
        problems.extend(report.mismatches);
        return Err(format!(
            "schema is not verified:\n  - {}",
            problems.join("\n  - ")
        ));
    }
    if report.ok() {
        println!(
            "Schema matches committed history ({} models).",
            history.models.len()
        );
        Ok(())
    } else {
        Err(format!(
            "schema differs from history:\n  - {}",
            report.mismatches.join("\n  - ")
        ))
    }
}

fn check<S>(registry: &Registry<S>) {
    let apps: Vec<&str> = registry.apps().iter().map(|a| &*a.id).collect();
    let operations: Vec<&str> = registry.operations().iter().map(|o| &*o.info.id).collect();
    let forms: Vec<&str> = registry
        .forms()
        .iter()
        .map(|f| f.form.id.as_str())
        .collect();
    let migrations: Vec<String> = registry
        .required_migrations()
        .iter()
        .map(ToString::to_string)
        .collect();
    let routes = registry.routes();
    let count = |f: fn(&RouteKind) -> bool| routes.iter().filter(|r| f(&r.kind)).count();
    let fingerprints = registry.fingerprints(Audience::Product);
    println!("Registry OK: {} {}", registry.name(), registry.version());
    println!("  apps (lifecycle order): {}", apps.join(", "));
    println!("  operations:             {}", operations.join(", "));
    println!("  forms:                  {}", forms.join(", "));
    println!(
        "  routes:                 {} managed, {} unmanaged, {} system",
        count(|k| matches!(k, RouteKind::Managed { .. })),
        count(|k| matches!(k, RouteKind::Unmanaged { .. })),
        count(|k| matches!(k, RouteKind::System)),
    );
    println!("  required migrations:    {}", migrations.join(", "));
    println!(
        "  contracts digest:       {}",
        fingerprints["contracts"].as_str().unwrap_or("")
    );
    println!(
        "  interactions digest:    {}",
        fingerprints["interactions"].as_str().unwrap_or("")
    );
}

fn inspect<S>(registry: &Registry<S>, what: Inspect) -> Result<(), String> {
    match what {
        Inspect::Apps => {
            println!(
                "{:<3} {:<10} {:<18} {:<32} SCHEMA",
                "#", "APP", "REQUIRES", "OPERATIONS"
            );
            for (i, app) in registry.apps().iter().enumerate() {
                let schema: Vec<&str> = app.schema.iter().map(|m| m.id).collect();
                println!(
                    "{:<3} {:<10} {:<18} {:<32} {}",
                    i + 1,
                    app.id,
                    dash(&app.requires.join(", ")),
                    dash(&app.operations.join(", ")),
                    schema.join(", ")
                );
            }
        }
        Inspect::Routes => {
            for route in registry.routes() {
                let kind = match route.kind {
                    RouteKind::System => "system".to_owned(),
                    RouteKind::Managed { operation } => format!("operation {operation}"),
                    RouteKind::Unmanaged { app, waiver } => {
                        format!("UNMANAGED ({app}) waiver: {waiver}")
                    }
                };
                println!("{:<7} {:<26} {kind}", route.method, route.path);
            }
        }
        Inspect::Operation { id } => {
            let described = registry
                .inspect_operation(&id)
                .ok_or_else(|| format!("no operation {id:?} is registered"))?;
            print!("{}", to_pretty(&described));
        }
        Inspect::Effects { model } => {
            if let Some(model) = &model
                && registry.desired_schema().model(model).is_none()
            {
                return Err(format!("no model {model:?} is installed"));
            }
            println!(
                "{:<28} {:<6} {:<24} {:<24} {:<24} EGRESS",
                "OPERATION", "MODE", "READS", "WRITES", "EMITS"
            );
            let list = |items: &[Arc<str>]| {
                let joined: Vec<&str> = items.iter().map(|i| &**i).collect();
                dash(&joined.join(", ")).to_owned()
            };
            for op in registry.operations() {
                let effects = op.info.effects.as_deref();
                if let Some(model) = &model
                    && !effects.is_some_and(|e| e.touches(model))
                {
                    continue;
                }
                match effects {
                    Some(e) => println!(
                        "{:<28} {:<6} {:<24} {:<24} {:<24} {}",
                        op.info.id,
                        op.info.mode.as_str(),
                        list(&e.reads),
                        list(&e.writes),
                        list(&e.emits),
                        list(&e.egress)
                    ),
                    None => println!(
                        "{:<28} {:<6} (not declared)",
                        op.info.id,
                        op.info.mode.as_str()
                    ),
                }
            }
        }
    }
    Ok(())
}

fn dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

async fn account<S>(ctx: &CommandContext<S>, action: AccountCommand) -> Result<(), String> {
    let env = ctx.env;
    // Mail settings are checked before anything is created.
    let mailer = |link: &LinkArgs| link.send.then(|| link.mail.mailer(env)).transpose();
    let pool = ctx.connect().await?;
    let hours = |h: u32| Duration::from_secs(u64::from(h) * 3600);
    match action {
        AccountCommand::Invite {
            email,
            valid_hours,
            link,
        } => {
            let origin = link.origin()?;
            let mailer = mailer(&link)?;
            let issued = password::invite(&pool, &email, hours(valid_hours))
                .await
                .map_err(|e| e.to_string())?;
            deliver(&ctx.name, &issued, origin, mailer).await?;
        }
        AccountCommand::ResetLink {
            email,
            valid_hours,
            link,
        } => {
            let origin = link.origin()?;
            let mailer = mailer(&link)?;
            let issued = password::issue_reset(&pool, &email, hours(valid_hours))
                .await
                .map_err(|e| e.to_string())?;
            deliver(&ctx.name, &issued, origin, mailer).await?;
        }
        AccountCommand::Disable { email } => {
            let actor = password::set_disabled(&pool, &email, true)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "Disabled {email} (actor {actor}); its sessions, links and API keys are void."
            );
        }
        AccountCommand::Enable { email } => {
            let actor = password::set_disabled(&pool, &email, false)
                .await
                .map_err(|e| e.to_string())?;
            println!("Enabled {email} (actor {actor}).");
        }
        AccountCommand::List => {
            for account in password::list(&pool).await.map_err(|e| e.to_string())? {
                println!(
                    "{:<9} {:<36} {} {}",
                    account.status, account.actor, account.created_at, account.email
                );
            }
        }
    }
    Ok(())
}

/// Emails an account link, or prints it for the operator to pass on.
pub async fn deliver(
    product: &str,
    issued: &password::Issued,
    origin: &str,
    mailer: Option<Arc<dyn Mailer>>,
) -> Result<(), String> {
    let what = match issued.purpose {
        password::Purpose::Invite => "Invite",
        password::Purpose::Reset => "Password reset",
    };
    let link = issued.link(origin);
    let Some(mailer) = mailer else {
        println!(
            "{what} link for {} (actor {}), valid until {}:\n\n  {link}\n\n\
             It works once. Send it to that person only.",
            issued.email, issued.actor, issued.expires_at
        );
        return Ok(());
    };
    mailer
        .send(&crate::mail::account_link(product, issued, &link))
        .await
        .map_err(|e| {
            format!(
                "{e}. The link was issued but not sent; run the command again \
                 (it replaces the link) or omit --send to print it."
            )
        })?;
    println!(
        "{what} link for {} (actor {}) sent via {}; valid until {}.",
        issued.email,
        issued.actor,
        mailer.describe(),
        issued.expires_at
    );
    Ok(())
}

/// A small pool for operator commands.
pub async fn connect(options: PgConnectOptions) -> Result<PgPool, String> {
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .map_err(|e| format!("connect to database: {e}"))
}

async fn connect_one(options: &PgConnectOptions) -> Result<sqlx::PgConnection, Failure> {
    use sqlx::Connection;
    sqlx::PgConnection::connect_with(options)
        .await
        .map_err(|e| Failure::new(exit::UNREACHABLE, format!("connect to database: {e}")))
}

async fn migrate<S>(
    ctx: &CommandContext<S>,
    options: PgConnectOptions,
    set: MigrationSet,
    args: MigrateArgs,
) -> Result<(), Failure> {
    if args.plan {
        let mut conn = connect_one(&options).await?;
        let report = reconcile::plan_report(&mut conn, &set, &ctx.name).await?;
        if args.json {
            println!("{}", serde_json::to_string(&report).expect("serializes"));
        } else {
            if report.steps.is_empty() && report.blocked.is_empty() {
                println!("Nothing to apply.");
            }
            for step in &report.steps {
                let resume = step
                    .resume_step
                    .map(|n| format!(", resumes at step {n}"))
                    .unwrap_or_default();
                println!(
                    "{}.{:<28} {:<8} {}{}{}",
                    step.app,
                    step.id,
                    step.class,
                    step.status,
                    if step.reasons.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", step.reasons.join(", "))
                    },
                    resume
                );
                for p in &step.preconditions {
                    let found = match (p.violations, &p.note) {
                        (Some(0), _) => "holds".to_owned(),
                        (Some(n), _) => format!("{n} violation(s)"),
                        (None, Some(note)) => format!("not evaluated: {note}"),
                        (None, None) => "not evaluated".to_owned(),
                    };
                    println!("    precondition {:?}: {found}", p.name);
                }
                for i in &step.impact {
                    let rows = i
                        .rows
                        .map(|n| format!("~{n} rows"))
                        .unwrap_or_else(|| "rows unknown".into());
                    let affected = match (i.affected, i.meaning, &i.note) {
                        (Some(n), Some(meaning), _) => format!(", {n} {meaning}"),
                        (None, _, Some(note)) => format!(", count {note}"),
                        _ => String::new(),
                    };
                    let blocks = if i.blocks { " (would fail)" } else { "" };
                    println!(
                        "    impact: {} on {} ({rows}){affected}{blocks}",
                        i.change, i.table
                    );
                }
            }
            for b in &report.blocked {
                println!("blocked: {b}");
            }
            if !report.steps.is_empty() {
                println!("plan digest: {}", report.plan_digest);
            }
        }
        return match report.exit_code() {
            exit::OK => Ok(()),
            code => Err(Failure::new(code, "the plan is blocked")),
        };
    }
    let gate = match (&args.approve, args.auto) {
        (Some(digest), _) => reconcile::Gate::Approved(digest.clone()),
        (None, true) => reconcile::Gate::Auto,
        (None, false) if ctx.env.is_production() => reconcile::Gate::Auto,
        (None, false) => reconcile::Gate::All,
    };
    reconcile::apply(
        &options,
        set,
        gate,
        args.backup_ref,
        args.lock_timeout,
        args.json,
        args.allow_drift,
    )
    .await
}

/// `serve` refuses to start on a partly applied migration or schema
/// drift. A database that is ahead of this build (a newer release applied
/// migrations) is allowed, as rolling deploys need.
/// Installs this build's schema fence (ADR-017) so its writes stop once a
/// newer release applies a fence migration this build lacks. Refuses to
/// start when that has already happened, or when `migrate` has not yet
/// created the fence.
async fn schema_fence_installed(
    options: &PgConnectOptions,
    set: &MigrationSet,
    bin: &str,
) -> Result<(), Failure> {
    use trestle_core::fence;
    let build = fence::BuildFence::of(set);
    if build.is_empty() {
        return Ok(());
    }
    let db = |e: sqlx::Error| Failure::new(exit::UNREACHABLE, format!("database: {e}"));
    let mut conn = connect_one(options).await?;
    if !fence::installed(&mut conn).await.map_err(db)? {
        return Err(Failure::new(
            exit::CHECK,
            format!(
                "refusing to start: the database has no schema fence yet (run `{bin} migrate`)"
            ),
        ));
    }
    let stale = fence::fenced_by(&mut conn, &build).await.map_err(db)?;
    if !stale.is_empty() {
        return Err(Failure::new(
            exit::DRIFT,
            format!(
                "refusing to start: a newer release fenced out this build; it does not contain {} \
                 (deploy a build that does)",
                stale.join(", ")
            ),
        ));
    }
    fence::install(build);
    Ok(())
}

async fn schema_verified(
    options: &PgConnectOptions,
    set: MigrationSet,
    bin: &str,
) -> Result<(), Failure> {
    let mut conn = connect_one(options).await?;
    let report = reconcile::verify(&mut conn, &set).await?;
    if !report.unknown_applied.is_empty() {
        tracing::warn!(
            newer = ?report.unknown_applied,
            "the database has migrations this build does not know; schema not compared"
        );
    }
    let mut problems = report.drift;
    problems.extend(
        report
            .partial
            .iter()
            .map(|p| format!("partially applied: {p}")),
    );
    if problems.is_empty() {
        return Ok(());
    }
    Err(Failure::new(
        exit::DRIFT,
        format!(
            "refusing to start: the database does not match committed history (see `{bin} verify`):\n  - {}",
            problems.join("\n  - ")
        ),
    ))
}

/// Where the intent comes from.
enum IntentSource {
    /// `$TRESTLE_INTENT`, else `./trestle.toml` if it exists.
    Default,
    File(PathBuf),
    Text(String, String),
    Doc(String, Box<crate::intent::IntentDoc>),
    None,
}

impl IntentSource {
    fn load(&self) -> Result<Option<(String, crate::intent::Intent)>, Vec<String>> {
        let (name, text) = match self {
            IntentSource::None => return Ok(None),
            IntentSource::Text(name, text) => (name.clone(), text.clone()),
            IntentSource::Doc(name, doc) => (name.clone(), doc.to_toml().map_err(|e| e.problems)?),
            IntentSource::File(path) => read_intent(path)?,
            IntentSource::Default => {
                let path = std::env::var_os(crate::intent::FILE_ENV)
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from);
                match path {
                    Some(path) => read_intent(&path)?,
                    None => {
                        let path = PathBuf::from(crate::intent::DEFAULT_FILE);
                        if !path.exists() {
                            return Ok(None);
                        }
                        read_intent(&path)?
                    }
                }
            }
        };
        crate::intent::parse(&name, &text)
            .map(|intent| Some((name, intent)))
            .map_err(|e| e.problems)
    }
}

fn read_intent(path: &Path) -> Result<(String, String), Vec<String>> {
    std::fs::read_to_string(path)
        .map(|text| (path.display().to_string(), text))
        .map_err(|e| vec![format!("{}: {e}", path.display())])
}

async fn tenant<S>(
    ctx: &CommandContext<S>,
    intent: Option<&Resolved>,
    action: TenantCommand,
) -> Result<(), String> {
    let tenancy = intent
        .and_then(|r| r.intent.tenancy.as_ref())
        .ok_or("trestle.toml declares no [tenancy]")?;
    let pool = ctx.connect().await?;
    let err = |e: sqlx::Error| e.to_string();
    match action {
        TenantCommand::Create { name } => {
            let id = Uuid::now_v7();
            sqlx::query(&format!(
                "INSERT INTO {} (id, name) VALUES ($1, $2)",
                tenancy.table()
            ))
            .bind(id)
            .bind(&name)
            .execute(&pool)
            .await
            .map_err(err)?;
            println!("{id}");
        }
        TenantCommand::Grant {
            tenant,
            actor,
            role,
        } => {
            let roles = &intent.expect("checked").intent.roles;
            if !roles.contains_key(&role) {
                let known: Vec<&str> = roles.keys().map(String::as_str).collect();
                return Err(format!(
                    "unknown role {role:?}; trestle.toml declares: {}",
                    known.join(", ")
                ));
            }
            sqlx::query(&format!(
                "INSERT INTO {table} ({col}, actor_id, role) VALUES ($1, $2, $3) \
                 ON CONFLICT ({col}, actor_id) DO UPDATE SET role = EXCLUDED.role",
                table = tenancy.membership_table(),
                col = tenancy.column()
            ))
            .bind(tenant)
            .bind(actor)
            .bind(&role)
            .execute(&pool)
            .await
            .map_err(err)?;
            println!("{actor} is now {role} of {tenant}.");
        }
        TenantCommand::Revoke { tenant, actor } => {
            let done = sqlx::query(&format!(
                "DELETE FROM {} WHERE {} = $1 AND actor_id = $2",
                tenancy.membership_table(),
                tenancy.column()
            ))
            .bind(tenant)
            .bind(actor)
            .execute(&pool)
            .await
            .map_err(err)?;
            if done.rows_affected() == 0 {
                println!("{actor} was not a member of {tenant}.");
            } else {
                println!("Removed {actor} from {tenant}.");
            }
        }
    }
    Ok(())
}

/// Intent apps declare no schema requirements by hand, so `serve` requires
/// every committed migration of those apps to be applied.
async fn intent_migrations_applied<S>(
    ctx: &CommandContext<S>,
    intent: &Resolved,
    set: MigrationSet,
) -> Result<(), String> {
    let mut apps: Vec<&str> = intent
        .intent
        .models
        .iter()
        .map(|m| m.app.as_str())
        .collect();
    if let Some(t) = &intent.intent.tenancy {
        apps.push(&t.name);
    }
    let required: Vec<trestle_historical::MigrationRef> = set
        .iter()
        .filter(|m| apps.contains(&m.app))
        .map(|m| m.key())
        .collect();
    let pool = ctx.connect().await?;
    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    let missing = trestle_historical::missing(&mut conn, &required)
        .await
        .map_err(|e| e.to_string())?;
    if missing.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = missing.iter().map(ToString::to_string).collect();
    Err(format!(
        "required migrations are not applied: {} (run `{} migrate`)",
        names.join(", "),
        ctx.bin
    ))
}
