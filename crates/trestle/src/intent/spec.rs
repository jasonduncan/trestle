//! The `trestle.toml` schema, and its validation into a resolved [`Intent`].
//!
//! The file is data only: no expressions, no conditionals (design §11).
//! Anything conditional is a named Rust handler or hook. Unknown keys are
//! errors, and every problem is reported with its line, all at once.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use toml::Spanned;
use trestle_historical::schema::{reserved_word, type_name};

/// Permission that any signed-in actor holds. Usable without `[tenancy]`.
pub const AUTHENTICATED: &str = "authenticated";

/// The file as read. [`super::doc::IntentDoc`] is the same schema as public
/// data: change both together (`tests/intent_doc.rs` reads a file that uses
/// every key).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    project: ProjectSection,
    #[serde(default)]
    auth: AuthSection,
    tenancy: Option<TenancySection>,
    #[serde(default)]
    roles: BTreeMap<String, Vec<Spanned<String>>>,
    #[serde(default)]
    models: Vec<ModelSection>,
    #[serde(default)]
    endpoints: Vec<EndpointSection>,
    #[serde(default)]
    admin: Vec<AdminSection>,
    #[serde(default)]
    views: Vec<super::doc::ViewDoc>,
}

/// `[[admin]]`: a model in the admin shell (ADR-029).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdminSection {
    model: Spanned<String>,
    /// Who sees it. Every button still runs its operation's own permission.
    permission: Option<Spanned<String>>,
    title: Option<String>,
    columns: Option<Vec<Spanned<String>>>,
    filters: Option<Vec<Spanned<String>>>,
    #[serde(default)]
    actions: Vec<Spanned<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectSection {
    name: String,
    /// The Trestle version the file was written for (informational).
    #[serde(default)]
    trestle: Option<String>,
}

/// `[auth]`: how people sign in. Names needs, never secrets.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    /// Email and password accounts (default on).
    #[serde(default = "yes")]
    pub passwords: bool,
    /// Opt-in OpenID Connect provider: `"google"`, or a table naming the
    /// issuer and the environment variables that hold the client ID and
    /// secret. Never the values themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<Oidc>,
}

/// `[auth] oidc`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Oidc {
    /// A well-known provider by name. Only `"google"` today.
    Named(String),
    Custom(OidcProvider),
}

/// `[auth.oidc]` as a table.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OidcProvider {
    pub issuer: String,
    /// Environment variable holding the client ID.
    pub client_id_env: String,
    /// Environment variable holding the client secret.
    pub client_secret_env: String,
}

impl Oidc {
    /// The issuer and the two environment variable names, or why the
    /// value is not usable.
    pub fn provider(&self) -> std::result::Result<OidcProvider, String> {
        let p = match self {
            Oidc::Named(name) if name == "google" => OidcProvider {
                issuer: "https://accounts.google.com".into(),
                client_id_env: "GOOGLE_CLIENT_ID".into(),
                client_secret_env: "GOOGLE_CLIENT_SECRET".into(),
            },
            Oidc::Named(name) => {
                return Err(format!(
                    "unknown OpenID Connect provider \"{name}\"; use \"google\" or a table \
                     {{ issuer = \"https://...\", client_id_env = \"...\", client_secret_env = \"...\" }}"
                ));
            }
            Oidc::Custom(p) => p.clone(),
        };
        if !p.issuer.starts_with("https://") {
            return Err(format!(
                "OpenID Connect issuer \"{}\" must be an https URL",
                p.issuer
            ));
        }
        for name in [&p.client_id_env, &p.client_secret_env] {
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !valid {
                return Err(format!(
                    "\"{name}\" is not an environment variable name; [auth] oidc names the \
                     variables that hold the client ID and secret, never their values"
                ));
            }
        }
        Ok(p)
    }
}

impl AuthSection {
    /// Whether people sign in in a browser: passwords, an OpenID Connect
    /// provider, or both. The project then installs the identity app
    /// (accounts, sessions and the `/api/v1/auth/*` routes, ADR-034).
    pub fn sign_in(&self) -> bool {
        self.passwords || self.oidc.is_some()
    }
}

fn yes() -> bool {
    true
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            passwords: true,
            oidc: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TenancySection {
    model: Spanned<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSection {
    id: Spanned<String>,
    #[serde(default)]
    table: Option<Spanned<String>>,
    #[serde(default)]
    fields: Vec<FieldSection>,
    /// Field sets that must be unique (per tenant when [tenancy] is set).
    #[serde(default)]
    unique: Vec<Spanned<Vec<String>>>,
    /// The field that names a row in relation pickers (ADR-026).
    #[serde(default)]
    label: Option<String>,
    /// Write hooks that run on every write to the model (ADR-039).
    #[serde(default)]
    hooks: BTreeMap<String, Spanned<String>>,
    /// Row scopes that scoped grants name (ADR-040).
    #[serde(default)]
    scopes: BTreeMap<String, Spanned<super::doc::ScopeDoc>>,
    /// Keep an append-only change log of every write (ADR-041).
    #[serde(default)]
    history: Option<Spanned<bool>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldSection {
    id: Spanned<String>,
    #[serde(rename = "type")]
    ty: Spanned<String>,
    #[serde(default)]
    min: Option<i64>,
    #[serde(default)]
    max: Option<i64>,
    #[serde(default)]
    optional: bool,
    #[serde(default)]
    default: Option<toml::Value>,
    #[serde(default)]
    values: Vec<String>,
    /// Enum fields: value → what people see (ADR-035).
    #[serde(default)]
    labels: BTreeMap<String, Spanned<String>>,
    #[serde(default)]
    precision: Option<u8>,
    #[serde(default)]
    scale: Option<u8>,
    /// For `ref` fields: the model referred to.
    #[serde(default)]
    to: Option<Spanned<String>>,
    /// What a generated form calls the field.
    #[serde(default)]
    label: Option<String>,
    /// Money fields: the currencies accepted (default: any code).
    #[serde(default)]
    currencies: Vec<String>,
    /// `"always"` (the default) or `"actions"`: written only by domain
    /// actions (ADR-037).
    #[serde(default)]
    writable: Option<Spanned<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointSection {
    id: Option<Spanned<String>>,
    model: Option<Spanned<String>>,
    #[serde(default)]
    crud: Vec<Spanned<String>>,
    read: Option<Spanned<String>>,
    write: Option<Spanned<String>>,
    #[serde(default)]
    filter: Vec<Spanned<String>>,
    order: Option<Spanned<String>>,
    search: Option<Spanned<String>>,
    #[serde(default)]
    hooks: BTreeMap<String, Spanned<String>>,
    path: Option<Spanned<String>>,
    permission: Option<Spanned<String>>,
    handler: Option<Spanned<String>>,
    #[serde(default)]
    input: Vec<FieldSection>,
    /// Domain actions: fields the action sets, the states it may start
    /// from, and fields the caller may supply.
    #[serde(default)]
    set: BTreeMap<String, toml::Value>,
    #[serde(default)]
    when: BTreeMap<String, Vec<toml::Value>>,
    #[serde(default)]
    accept: Vec<String>,
    /// Guarded actions (ADR-036): fields the written row must have, the
    /// comparisons it must pass, related rows that must exist, and what
    /// asking again for the same state does.
    #[serde(default)]
    require: Vec<String>,
    #[serde(default)]
    check: Vec<super::doc::CheckDoc>,
    #[serde(default)]
    exists: Vec<super::doc::ExistsDoc>,
    repeat: Option<Spanned<String>>,
    /// Publish generated forms for this endpoint's writes (ADR-026).
    #[serde(default)]
    forms: bool,
    /// Its writes accept an `Idempotency-Key` (ADR-016, ADR-038).
    #[serde(default)]
    idempotent: bool,
    /// Publish `GET <path>/{id}/history`, the model's change log (ADR-041).
    #[serde(default)]
    history: bool,
}

/// A validated intent file.
#[derive(Debug, Clone)]
pub struct Intent {
    pub name: String,
    pub trestle: Option<String>,
    pub auth: AuthSection,
    pub tenancy: Option<Tenancy>,
    /// Role → the permissions it holds on every row of the tenant. Every
    /// role is a key, including one whose grants are all scoped.
    pub roles: BTreeMap<String, BTreeSet<String>>,
    /// Role → permission → scopes, for a permission the role holds only on
    /// the rows of those scopes (`"crm.sell@own"`, ADR-040). A permission
    /// in `roles` for the same role is never here: an unscoped grant
    /// admits every row.
    pub scoped: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// Shared with the operations and rows built from them, so the intent
    /// lives exactly as long as something uses it.
    pub models: Vec<Arc<ModelSpec>>,
    pub endpoints: Vec<EndpointSpec>,
    pub admin: Vec<AdminSpec>,
    pub views: Vec<super::doc::ViewDoc>,
}

impl Intent {
    /// What parses but is likely a mistake. The project logs each one when
    /// it loads the file.
    ///
    /// Today that is a scoped model's unique set without the field of a
    /// scope that a grant uses. Within that scope the set is checked
    /// against rows the caller can't see, so a write learns whether a
    /// hidden row holds its value (ADR-040).
    pub fn warnings(&self) -> Vec<String> {
        let granted: BTreeSet<&str> = self
            .scoped
            .values()
            .flat_map(|grants| grants.values().flatten())
            .map(String::as_str)
            .collect();
        let mut warnings = Vec::new();
        for model in &self.models {
            for set in &model.unique {
                let missing: Vec<String> = model
                    .scopes
                    .iter()
                    .filter(|(name, scope)| {
                        granted.contains(name.as_str()) && !set.fields.contains(&scope.field)
                    })
                    .map(|(name, scope)| format!("{name} ({})", scope.field))
                    .collect();
                if !missing.is_empty() {
                    let scopes = if missing.len() == 1 {
                        "scope"
                    } else {
                        "scopes"
                    };
                    warnings.push(format!(
                        "unique set {:?} of {} leaves out the field of {scopes} {}, so a caller \
                         granted one learns whether a row it can't see holds the value \
                         (ADR-040). Add the field to the set if the values are private to the \
                         scope",
                        set.fields,
                        model.id,
                        missing.join(", ")
                    ));
                }
            }
        }
        warnings
    }

    /// Whether some endpoint accepts an `Idempotency-Key`. The project then
    /// installs `trestle_core::idempotency::IdempotencyApp` (ADR-038).
    pub fn idempotent(&self) -> bool {
        self.endpoints.iter().any(EndpointSpec::idempotent)
    }
}

/// A model shown in the admin shell (ADR-029): its CRUD endpoint's list,
/// forms for its create and update, and buttons for its actions.
#[derive(Debug, Clone)]
pub struct AdminSpec {
    pub model: String,
    /// The permission that shows the model in the admin shell.
    pub permission: String,
    pub title: String,
    /// Item fields shown as list columns, in order.
    pub columns: Vec<String>,
    /// List filters offered (a subset of the endpoint's `filter`).
    pub filters: Vec<String>,
    /// IDs of action endpoints on the model, shown as buttons.
    pub actions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Tenancy {
    /// `workspace`: app `workspace`, tables `workspaces` and
    /// `workspace_memberships`, column `workspace_id` on every model.
    pub name: String,
}

impl Tenancy {
    pub fn table(&self) -> String {
        format!("{}s", self.name)
    }
    pub fn membership_table(&self) -> String {
        format!("{}_memberships", self.name)
    }
    pub fn column(&self) -> String {
        format!("{}_id", self.name)
    }
    pub fn model_id(&self) -> String {
        format!("{0}.{0}", self.name)
    }
    pub fn membership_model_id(&self) -> String {
        format!("{}.membership", self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Text,
    Int,
    Bool,
    Uuid,
    Enum,
    /// `precision` and `scale` on the field (ADR-022, ADR-023).
    Decimal,
    Date,
    Instant,
    Duration,
    /// The ID of another model's row in the same tenant (`to = "<model>"`).
    Ref,
    /// `{amount, currency}`, stored as `<id>_amount` and `<id>_currency`
    /// (ADR-027).
    Money,
}

impl FieldType {
    pub fn sql(self) -> &'static str {
        match self {
            FieldType::Text | FieldType::Enum => "text",
            FieldType::Int => "bigint",
            FieldType::Bool => "boolean",
            FieldType::Uuid | FieldType::Ref => "uuid",
            FieldType::Decimal | FieldType::Money => "numeric",
            FieldType::Date => "date",
            FieldType::Instant => "timestamptz",
            FieldType::Duration => "interval",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub id: String,
    pub ty: FieldType,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub optional: bool,
    /// A database default; `"now"` on an instant is `DEFAULT now()`
    /// ([`FieldSpec::defaults_to_now`], ADR-043).
    pub default: Option<serde_json::Value>,
    pub values: Vec<String>,
    /// Enum fields: display labels for some values (ADR-035). Presentation
    /// only: the stored and API value stays the value.
    pub labels: BTreeMap<String, String>,
    /// Decimal fields: total digits, and digits after the point.
    pub precision: u8,
    pub scale: u8,
    /// Ref fields: the model referred to, and its table.
    pub to: Option<String>,
    pub to_table: Option<String>,
    /// Form label; defaults to the ID in words.
    pub label: Option<String>,
    /// Money fields: allowed currencies; empty means any.
    pub currencies: Vec<String>,
    /// Which writes may change the field (ADR-037).
    pub writable: Writable,
}

/// Which writes may change a model field (`writable = ...`, ADR-037).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Writable {
    /// Any write: CRUD create and update, handlers, hooks and actions.
    #[default]
    Always,
    /// Only domain actions (`set` and `accept`). CRUD contracts and forms
    /// leave it out, and handlers and hooks are refused it.
    Actions,
}

impl FieldSpec {
    /// Required on create: not optional and no default.
    pub fn required(&self) -> bool {
        !self.optional && self.default.is_none()
    }

    /// Written only by domain actions (`writable = "actions"`).
    pub fn managed(&self) -> bool {
        self.writable == Writable::Actions
    }

    /// `default = "now"` on an instant: a create that leaves it out stores
    /// the transaction's time, through the column's `DEFAULT now()`
    /// (ADR-043).
    pub fn defaults_to_now(&self) -> bool {
        self.ty == FieldType::Instant && self.default.as_ref().and_then(|v| v.as_str()) == Some(NOW)
    }
}

#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// `notes.note`
    pub id: String,
    pub app: String,
    pub table: String,
    pub fields: Vec<FieldSpec>,
    /// Unique field sets, scoped per tenant, with their constraint names.
    pub unique: Vec<UniqueSpec>,
    /// The field relation pickers show for a row of this model.
    pub label: Option<String>,
    /// Write hook point (`before_write`, `after_write`) → its Rust name
    /// (ADR-039). They run on every write path to the model.
    pub hooks: BTreeMap<String, Named>,
    /// Row scopes (ADR-040): name → how a row is tied to the acting actor.
    pub scopes: BTreeMap<String, ScopeSpec>,
    /// Every write appends a row to the change log, [`ModelSpec::history_table`]
    /// (ADR-041).
    pub history: bool,
}

/// A row scope (ADR-040): the rows of a model tied to the acting actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSpec {
    /// The field of the row: a `uuid` holding the actor itself, or, with
    /// `through`, the value a `through` row's `key` must equal.
    pub field: String,
    pub through: Option<Through>,
}

/// `through = "<model>"` on a scope: a row of that model, in the same
/// tenant, links the scoped row to an actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Through {
    pub model: String,
    pub table: String,
    /// Its field (or `id`) that equals the scoped row's field.
    pub key: String,
    /// Its `uuid` field holding the actor.
    pub actor: String,
    /// `where` (ADR-044): its field → the constants (JSON) one of which
    /// the field must equal, in field order. Empty admits any such row.
    pub conditions: Vec<(String, Vec<serde_json::Value>)>,
}

/// `unique = [["name"]]` on a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueSpec {
    /// `<table>_<tenant>_<fields>_key`, or `<table>_<fields>_key` without
    /// tenancy: Postgres's convention, with the tenant column's `_id`
    /// dropped.
    pub name: String,
    /// Declared fields, without the tenant column.
    pub fields: Vec<String>,
}

impl ModelSpec {
    pub fn field(&self, id: &str) -> Option<&FieldSpec> {
        self.fields.iter().find(|f| f.id == id)
    }
    /// Collection path: `/api/v1/<table>`.
    pub fn path(&self) -> String {
        format!("/api/v1/{}", self.table)
    }
    /// The change log's model ID (ADR-041): `crm.deal` → `crm.deal_history`.
    pub fn history_model(&self) -> String {
        format!("{}_history", self.id)
    }
    /// The change log's table (ADR-041): `deals` → `deals_history`.
    pub fn history_table(&self) -> String {
        format!("{}_history", self.table)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Crud {
    List,
    Get,
    Create,
    Update,
    Delete,
}

impl Crud {
    pub fn as_str(self) -> &'static str {
        match self {
            Crud::List => "list",
            Crud::Get => "get",
            Crud::Create => "create",
            Crud::Update => "update",
            Crud::Delete => "delete",
        }
    }
    pub fn writes(self) -> bool {
        matches!(self, Crud::Create | Crud::Update | Crud::Delete)
    }
}

/// Write hook points on a model (ADR-039): they run inside the write's
/// transaction on every write to it, whichever path makes the write.
pub const WRITE_HOOKS: &[&str] = &["before_write", "after_write"];

/// Hook points on CRUD endpoints. All run inside the operation's
/// transaction.
pub const HOOKS: &[&str] = &[
    "validate",
    "before_create",
    "after_create",
    "before_update",
    "after_update",
    "before_delete",
];

#[derive(Debug, Clone)]
pub enum EndpointSpec {
    Crud {
        model: String,
        ops: BTreeSet<Crud>,
        read: String,
        write: String,
        filter: Vec<String>,
        search: Option<String>,
        /// Column and descending.
        order: (String, bool),
        hooks: BTreeMap<String, Named>,
        forms: bool,
        /// Create, update and delete accept an `Idempotency-Key` (ADR-038).
        idempotent: bool,
        /// `GET <path>/{id}/history` reads the model's change log with the
        /// `read` permission (ADR-041).
        history: bool,
    },
    Custom {
        id: String,
        model: String,
        method: http::Method,
        path: String,
        permission: String,
        handler: Named,
        input: Vec<FieldSpec>,
        /// The binding accepts an `Idempotency-Key` (ADR-038); writes only.
        idempotent: bool,
    },
    /// A state transition on one row, declared without Rust (ADR-025).
    Action {
        id: String,
        model: String,
        path: String,
        permission: String,
        /// Field → value written; `Now` for the transaction's time.
        set: Vec<(String, SetValue)>,
        /// Field → the values it may hold for the action to run.
        when: Vec<(String, Vec<serde_json::Value>)>,
        /// Fields the caller may supply, copied onto the row.
        accept: Vec<String>,
        /// Fields that must be non-null in the row as written (ADR-036).
        require: Vec<String>,
        /// Comparisons the row as written must pass (ADR-036).
        check: Vec<CheckSpec>,
        /// Related rows that must exist in the tenant (ADR-036).
        exists: Vec<ExistsSpec>,
        /// What a request for the state the row already has does.
        repeat: Repeat,
        forms: bool,
        /// The binding accepts an `Idempotency-Key` (ADR-038).
        idempotent: bool,
    },
}

impl EndpointSpec {
    /// `idempotent = true`: the endpoint's write bindings accept an
    /// `Idempotency-Key` (ADR-016, ADR-038).
    pub fn idempotent(&self) -> bool {
        match self {
            EndpointSpec::Crud { idempotent, .. }
            | EndpointSpec::Custom { idempotent, .. }
            | EndpointSpec::Action { idempotent, .. } => *idempotent,
        }
    }
}

/// A comparison operator of an action `check` (ADR-036).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compare {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Compare {
    pub const ALL: [Compare; 6] = [
        Compare::Eq,
        Compare::Ne,
        Compare::Lt,
        Compare::Le,
        Compare::Gt,
        Compare::Ge,
    ];

    /// The key in the file: `eq`, `ne`, `lt`, `le`, `gt` or `ge`.
    pub fn as_str(self) -> &'static str {
        match self {
            Compare::Eq => "eq",
            Compare::Ne => "ne",
            Compare::Lt => "lt",
            Compare::Le => "le",
            Compare::Gt => "gt",
            Compare::Ge => "ge",
        }
    }

    /// The SQL operator.
    pub fn sql(self) -> &'static str {
        match self {
            Compare::Eq => "=",
            Compare::Ne => "<>",
            Compare::Lt => "<",
            Compare::Le => "<=",
            Compare::Gt => ">",
            Compare::Ge => ">=",
        }
    }

    /// Whether it orders values (anything but `eq` and `ne`).
    pub fn orders(self) -> bool {
        !matches!(self, Compare::Eq | Compare::Ne)
    }
}

/// `check = [{ field = "value", ge = "0" }]` on an action.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckSpec {
    pub field: String,
    pub op: Compare,
    /// The constant, as JSON: a string for decimals, dates, instants and
    /// durations; for money, a decimal string (the amount, whatever the
    /// currency) or `{amount, currency}`.
    pub value: serde_json::Value,
}

/// `exists = [{ model = "...", match = {...}, null = [...] }]` on an
/// action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistsSpec {
    /// The related model.
    pub model: String,
    /// The related model's field → this model's field (either may be
    /// `id`), in the related field's order.
    pub matches: Vec<(String, String)>,
    /// Fields of the related row that must be null.
    pub null: Vec<String>,
}

/// `repeat = ...` on an action (ADR-036).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Repeat {
    /// A second request fails its `when` rule as usual (409).
    #[default]
    Refuse,
    /// When every `set` constant already holds, the action changes nothing
    /// and returns the row (200).
    Unchanged,
}

/// What an action writes into a field.
#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    Value(serde_json::Value),
    /// `"now"` on an instant field.
    Now,
}

/// A handler or hook name and where the file names it.
#[derive(Debug, Clone)]
pub struct Named {
    pub name: String,
    pub line: usize,
}

/// Everything wrong with an intent file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentError {
    pub problems: Vec<String>,
}

impl std::fmt::Display for IntentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "intent file has {} problem(s):", self.problems.len())?;
        for p in &self.problems {
            writeln!(f, "  - {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for IntentError {}

struct Lines<'a> {
    file: &'a str,
    text: &'a str,
}

impl Lines<'_> {
    fn line(&self, span: Range<usize>) -> usize {
        self.text[..span.start.min(self.text.len())]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1
    }
    fn at<T>(&self, spanned: &Spanned<T>) -> String {
        format!("{}:{}", self.file, self.line(spanned.span()))
    }
}

fn ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 50
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

const RESERVED: &[&str] = &["id", "version", "created_at", "expected_version", "limit"];

/// `"now"`: the transaction's time, in an action's `set` (ADR-025) and as an
/// instant field's `default` (ADR-043).
pub const NOW: &str = "now";

/// Why `table` can't hold an intent model, if it can't (ADR-042): it is a
/// reserved SQL word, or a name intent SQL would read as a type when it
/// casts a record to the table (`NULL::<table>`). The lists live in
/// `trestle_historical::schema`.
fn unusable_table(table: &str, instead: &str) -> Option<String> {
    let suggestion = if reserved_word(&format!("{table}s")) || type_name(&format!("{table}s")) {
        instead.to_owned()
    } else {
        format!("{table}s")
    };
    if reserved_word(table) {
        Some(format!(
            "is a reserved SQL word; choose another table, e.g. table = {suggestion:?}"
        ))
    } else if type_name(table) {
        Some(format!(
            "is a PostgreSQL type name, which SQL would read as the type; choose another \
             table, e.g. table = {suggestion:?}"
        ))
    } else {
        None
    }
}

/// Parses and validates `text` (named `file` in messages).
pub fn parse(file: &str, text: &str) -> Result<Intent, IntentError> {
    let lines = Lines { file, text };
    let raw: File = toml::from_str(text).map_err(|e| {
        let line = e.span().map(|s| lines.line(s)).unwrap_or(1);
        IntentError {
            problems: vec![format!("{file}:{line}: {}", e.message())],
        }
    })?;
    let mut problems = Vec::new();

    let tenancy = raw.tenancy.as_ref().map(|t| {
        if !ident(t.model.get_ref()) {
            problems.push(format!(
                "{}: tenancy model {:?} must be a lowercase identifier",
                lines.at(&t.model),
                t.model.get_ref()
            ));
        }
        let tenancy = Tenancy {
            name: t.model.get_ref().clone(),
        };
        let table = tenancy.table();
        if unusable_table(&table, "").is_some() {
            problems.push(format!(
                "{}: tenancy model {:?} stores tenants in table {table:?}, which is a reserved \
                 SQL word or type name; choose another tenancy model",
                lines.at(&t.model),
                t.model.get_ref()
            ));
        }
        tenancy
    });

    let mut roles: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut scoped: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    let mut permissions = BTreeSet::new();
    // Scoped grants are checked against the models' scopes below.
    let mut scope_grants = Vec::new();
    for (role, perms) in &raw.roles {
        if !ident(role) {
            problems.push(format!(
                "{file}: role {role:?} must be a lowercase identifier"
            ));
        }
        let mut set = BTreeSet::new();
        for grant in perms {
            let text = grant.get_ref();
            match text.split_once('@') {
                None => {
                    set.insert(text.clone());
                }
                Some((permission, scope)) => {
                    if permission.is_empty() || !ident(scope) {
                        problems.push(format!(
                            "{}: grant {text:?} must be \"<permission>@<scope>\", with a \
                             lowercase scope name",
                            lines.at(grant)
                        ));
                        continue;
                    }
                    if permission == AUTHENTICATED {
                        problems.push(format!(
                            "{}: \"{AUTHENTICATED}\" is held by every signed-in actor; it cannot \
                             be granted for a scope",
                            lines.at(grant)
                        ));
                        continue;
                    }
                    scope_grants.push((grant, scope.to_owned()));
                    scoped
                        .entry(role.clone())
                        .or_default()
                        .entry(permission.to_owned())
                        .or_default()
                        .insert(scope.to_owned());
                }
            }
        }
        permissions.extend(set.iter().cloned());
        roles.insert(role.clone(), set);
    }
    // An unscoped grant admits every row, so it absorbs scoped ones.
    for (role, grants) in &mut scoped {
        let all = &roles[role];
        grants.retain(|permission, _| !all.contains(permission));
        permissions.extend(grants.keys().cloned());
    }
    scoped.retain(|_, grants| !grants.is_empty());
    if !roles.is_empty() && tenancy.is_none() {
        problems.push(format!(
            "{file}: [roles] needs [tenancy]: roles are granted per tenant membership"
        ));
    }

    let mut models = Vec::new();
    let mut tables = BTreeSet::new();
    for m in &raw.models {
        let at = lines.at(&m.id);
        let id = m.id.get_ref();
        let Some((app, name)) = id.split_once('.').filter(|(a, n)| ident(a) && ident(n)) else {
            problems.push(format!(
                "{at}: model id {id:?} must be \"<app>.<name>\" in lowercase (e.g. \"notes.note\")"
            ));
            continue;
        };
        if let Some(t) = &tenancy
            && app == t.name
        {
            problems.push(format!(
                "{at}: app {app:?} is the tenancy model's app; use another app name"
            ));
        }
        let table_at = m.table.as_ref().map_or_else(|| at.clone(), |t| lines.at(t));
        let table = m
            .table
            .as_ref()
            .map_or_else(|| format!("{name}s"), |t| t.get_ref().clone());
        if !ident(&table) {
            problems.push(format!(
                "{table_at}: table {table:?} must be a lowercase identifier"
            ));
        } else if let Some(problem) = unusable_table(&table, &format!("{app}_{table}")) {
            problems.push(format!("{table_at}: table {table:?} {problem}"));
        }
        if !tables.insert(table.clone()) {
            problems.push(format!("{at}: table {table:?} is used twice"));
        }
        if models.iter().any(|x: &Arc<ModelSpec>| x.id == *id) {
            problems.push(format!("{at}: model {id:?} is declared twice"));
        }
        let mut fields = Vec::new();
        for f in &m.fields {
            if let Some(spec) = field(&lines, f, &mut problems) {
                if fields.iter().any(|x: &FieldSpec| x.id == spec.id) {
                    problems.push(format!(
                        "{}: field {:?} is declared twice",
                        lines.at(&f.id),
                        spec.id
                    ));
                }
                if reserved_word(&spec.id) {
                    let suggestion = match spec.ty {
                        FieldType::Bool => format!("is_{}", spec.id),
                        FieldType::Ref | FieldType::Uuid => format!("{}_id", spec.id),
                        FieldType::Instant => format!("{}_at", spec.id),
                        FieldType::Date => format!("{}_on", spec.id),
                        _ => format!("{name}_{}", spec.id),
                    };
                    problems.push(format!(
                        "{}: field {:?} is a reserved SQL word; choose another ID, e.g. {suggestion:?}",
                        lines.at(&f.id),
                        spec.id
                    ));
                }
                if let Some(t) = &tenancy
                    && spec.id == t.column()
                {
                    problems.push(format!(
                        "{}: field {:?} is added by [tenancy]; remove it",
                        lines.at(&f.id),
                        spec.id
                    ));
                }
                fields.push(spec);
            }
        }
        for money in fields.iter().filter(|f| f.ty == FieldType::Money) {
            for column in [
                format!("{}_amount", money.id),
                format!("{}_currency", money.id),
            ] {
                if fields.iter().any(|f| f.id == column) {
                    problems.push(format!(
                        "{at}: field {column:?} collides with the storage of money field {:?}",
                        money.id
                    ));
                }
            }
        }
        let mut unique = Vec::new();
        for set in &m.unique {
            let at = lines.at(set);
            let names = set.get_ref();
            if names.is_empty() {
                problems.push(format!("{at}: a unique set needs at least one field"));
                continue;
            }
            for n in names {
                match fields.iter().find(|f: &&FieldSpec| &f.id == n) {
                    None => {
                        problems.push(format!("{at}: unique field {n:?} is not a field of {id}"))
                    }
                    Some(f) if f.ty == FieldType::Money => {
                        problems.push(format!(
                            "{at}: money field {n:?} cannot be part of a unique set"
                        ));
                    }
                    Some(_) => {}
                }
            }
            let mut parts = vec![table.clone()];
            if let Some(t) = &tenancy {
                parts.push(t.name.clone());
            }
            parts.extend(names.iter().cloned());
            parts.push("key".into());
            let name = parts.join("_");
            if name.len() > 63 {
                problems.push(format!(
                    "{at}: constraint name {name:?} is longer than 63 characters; use fewer fields"
                ));
            }
            unique.push(UniqueSpec {
                name,
                fields: names.clone(),
            });
        }
        let label = match &m.label {
            Some(l)
                if !fields
                    .iter()
                    .any(|f: &FieldSpec| &f.id == l && f.ty == FieldType::Text) =>
            {
                problems.push(format!("{at}: label {l:?} must name a text field of {id}"));
                None
            }
            Some(l) => Some(l.clone()),
            // The first text field, if any.
            None => fields
                .iter()
                .find(|f| f.ty == FieldType::Text)
                .map(|f| f.id.clone()),
        };
        let mut hooks = BTreeMap::new();
        for (point, name) in &m.hooks {
            if !WRITE_HOOKS.contains(&point.as_str()) {
                problems.push(format!(
                    "{}: unknown write hook {point:?} on {id}; use {}",
                    lines.at(name),
                    WRITE_HOOKS.join(" or ")
                ));
                continue;
            }
            if name.get_ref().is_empty() {
                problems.push(format!(
                    "{}: {point} hook of {id} needs a name",
                    lines.at(name)
                ));
                continue;
            }
            hooks.insert(
                point.clone(),
                Named {
                    name: name.get_ref().clone(),
                    line: lines.line(name.span()),
                },
            );
        }
        models.push(Arc::new(ModelSpec {
            id: id.clone(),
            app: app.to_owned(),
            table,
            fields,
            unique,
            label,
            hooks,
            scopes: BTreeMap::new(),
            history: m.history.as_ref().is_some_and(|h| *h.get_ref()),
        }));
    }
    // A change log is a table of its own (ADR-041), so its names must be
    // free.
    for model in models.iter().filter(|m| m.history) {
        let Some(raw_model) = raw.models.iter().find(|m| *m.id.get_ref() == model.id) else {
            continue;
        };
        let at = raw_model
            .history
            .as_ref()
            .map(|h| lines.at(h))
            .unwrap_or_else(|| lines.at(&raw_model.id));
        let (id, table) = (model.history_model(), model.history_table());
        if models.iter().any(|m| m.id == id) || tables.contains(&table) {
            problems.push(format!(
                "{at}: history of {} is stored as model {id:?} in table {table:?}, which this \
                 file already uses; rename one of them",
                model.id
            ));
        }
        if model.fields.is_empty() {
            problems.push(format!(
                "{at}: history of {} records changed fields, and it has none",
                model.id
            ));
        }
    }
    // References resolve once every model is known.
    let targets: BTreeMap<String, String> = models
        .iter()
        .map(|m| (m.id.clone(), m.table.clone()))
        .collect();
    for model in &mut models {
        let model = Arc::make_mut(model);
        let raw_model = raw.models.iter().find(|m| *m.id.get_ref() == model.id);
        for field in &mut model.fields {
            let Some(to) = &field.to else { continue };
            match targets.get(to) {
                Some(table) => field.to_table = Some(table.clone()),
                None => {
                    let at = raw_model
                        .and_then(|m| m.fields.iter().find(|f| *f.id.get_ref() == field.id))
                        .and_then(|f| f.to.as_ref())
                        .map(|t| lines.at(t))
                        .unwrap_or_default();
                    problems.push(format!(
                        "{at}: field {:?} refers to {to:?}, which is not a model in this file",
                        field.id
                    ));
                }
            }
        }
    }

    // Scopes name fields of other models, so they resolve once every
    // model and ref is known (ADR-040).
    let resolved_models = models.clone();
    for model in &mut models {
        let Some(raw_model) = raw.models.iter().find(|m| *m.id.get_ref() == model.id) else {
            continue;
        };
        let mut scopes = BTreeMap::new();
        for (name, doc) in &raw_model.scopes {
            if let Some(spec) = scope_spec(
                &lines,
                (model, &resolved_models),
                name,
                doc,
                tenancy.is_some(),
                &mut problems,
            ) {
                scopes.insert(name.clone(), spec);
            }
        }
        Arc::make_mut(model).scopes = scopes;
    }
    for (grant, scope) in &scope_grants {
        if !raw.models.iter().any(|m| m.scopes.contains_key(scope)) {
            problems.push(format!(
                "{}: grant {:?} names scope {scope:?}, which no model declares; add \
                 scopes = {{ {scope} = {{ field = \"...\" }} }} to a model",
                lines.at(grant),
                grant.get_ref()
            ));
        }
    }

    let known_permission = |p: &str| p == AUTHENTICATED || permissions.contains(p);
    let check_permission = |p: &Spanned<String>, problems: &mut Vec<String>| {
        let name = p.get_ref();
        if !known_permission(name) {
            problems.push(format!(
                "{}: permission {name:?} is not granted by any role; add it to a role in [roles] \
                 or use \"{AUTHENTICATED}\"",
                lines.at(p)
            ));
        } else if name != AUTHENTICATED && tenancy.is_none() {
            problems.push(format!(
                "{}: permission {name:?} needs [tenancy]",
                lines.at(p)
            ));
        }
        name.clone()
    };

    let mut endpoints = Vec::new();
    let mut crud_models = BTreeSet::new();
    let mut custom_ids = BTreeSet::new();
    for (index, e) in raw.endpoints.iter().enumerate() {
        let where_ = e
            .model
            .as_ref()
            .map(|m| lines.at(m))
            .or_else(|| e.id.as_ref().map(|i| lines.at(i)))
            .unwrap_or_else(|| format!("{file}: endpoint #{}", index + 1));
        let action = !e.set.is_empty()
            || !e.when.is_empty()
            || !e.accept.is_empty()
            || !e.require.is_empty()
            || !e.check.is_empty()
            || !e.exists.is_empty()
            || e.repeat.is_some();
        if e.handler.is_some() || action {
            // A custom endpoint: Rust (handler) or a declared action.
            for (present, key) in [
                (!e.crud.is_empty(), "crud"),
                (e.read.is_some(), "read"),
                (e.write.is_some(), "write"),
                (!e.filter.is_empty(), "filter"),
                (e.order.is_some(), "order"),
                (!e.hooks.is_empty(), "hooks"),
                (e.forms && e.handler.is_some(), "forms"),
                (e.history, "history"),
            ] {
                if present {
                    problems.push(format!(
                        "{where_}: {key:?} is for CRUD endpoints; a handler endpoint takes id, path, permission, handler, model and input"
                    ));
                }
            }
            if e.handler.is_some() && action {
                problems.push(format!(
                    "{where_}: an endpoint is either a handler or an action (set, when, accept), not both"
                ));
                continue;
            }
            let Some(id) = &e.id else {
                problems.push(format!(
                    "{where_}: a handler endpoint needs an id (e.g. \"notes.pin\")"
                ));
                continue;
            };
            let id_text = id.get_ref();
            let Some((app, _)) = id_text
                .split_once('.')
                .filter(|(a, n)| ident(a) && ident(n))
            else {
                problems.push(format!(
                    "{}: endpoint id {id_text:?} must be \"<app>.<name>\"",
                    lines.at(id)
                ));
                continue;
            };
            if !custom_ids.insert(id_text.clone()) {
                problems.push(format!(
                    "{}: endpoint {id_text:?} is declared twice",
                    lines.at(id)
                ));
            }
            let model = match &e.model {
                Some(m) => m.get_ref().clone(),
                None => {
                    let in_app: Vec<&ModelSpec> = models
                        .iter()
                        .filter(|m| m.app == app)
                        .map(|m| &**m)
                        .collect();
                    match in_app.as_slice() {
                        [only] => only.id.clone(),
                        _ => {
                            problems.push(format!(
                                "{}: endpoint {id_text:?} returns a model; name it with model = \"{app}.<name>\"",
                                lines.at(id)
                            ));
                            continue;
                        }
                    }
                }
            };
            if !models.iter().any(|m| m.id == model) {
                problems.push(format!(
                    "{where_}: endpoint {id_text:?} names unknown model {model:?}"
                ));
            }
            let Some(path) = &e.path else {
                problems.push(format!(
                    "{}: endpoint {id_text:?} needs path = \"POST /api/v1/...\"",
                    lines.at(id)
                ));
                continue;
            };
            let (method, route) = match path.get_ref().split_once(' ') {
                Some((m, p)) if p.starts_with("/api/") => match m {
                    "GET" => (http::Method::GET, p),
                    "POST" => (http::Method::POST, p),
                    "PATCH" => (http::Method::PATCH, p),
                    "PUT" => (http::Method::PUT, p),
                    "DELETE" => (http::Method::DELETE, p),
                    _ => {
                        problems.push(format!("{}: unknown method {m:?}", lines.at(path)));
                        continue;
                    }
                },
                _ => {
                    problems.push(format!(
                        "{}: path must be \"<METHOD> /api/...\", e.g. \"POST /api/v1/notes/{{id}}/pin\"",
                        lines.at(path)
                    ));
                    continue;
                }
            };
            let Some(permission) = &e.permission else {
                problems.push(format!(
                    "{}: endpoint {id_text:?} needs a permission (there is no unprotected default)",
                    lines.at(id)
                ));
                continue;
            };
            let permission = check_permission(permission, &mut problems);
            let Some(handler) = &e.handler else {
                let Some(spec) = models.iter().find(|m| m.id == model) else {
                    continue;
                };
                if let Some(action) = action_spec(
                    &lines,
                    &where_,
                    (spec, &models),
                    e,
                    (id_text, &method, route, &permission),
                    &mut problems,
                ) {
                    endpoints.push(action);
                }
                continue;
            };
            if e.idempotent && method == http::Method::GET {
                problems.push(format!(
                    "{}: endpoint {id_text:?} is a GET, which only reads; idempotent = true is for writes",
                    lines.at(id)
                ));
            }
            let mut input = Vec::new();
            for f in &e.input {
                if let Some(w) = &f.writable {
                    problems.push(format!(
                        "{}: writable = ... is for model fields, not handler input",
                        lines.at(w)
                    ));
                }
                if let Some(spec) = field(&lines, f, &mut problems) {
                    if spec.defaults_to_now() {
                        problems.push(format!(
                            "{}: default = \"now\" is for model fields, which the database \
                             fills on create; a handler input has no row",
                            lines.at(&f.id)
                        ));
                    }
                    input.push(spec);
                }
            }
            endpoints.push(EndpointSpec::Custom {
                id: id_text.clone(),
                model,
                method,
                path: route.to_owned(),
                permission,
                handler: Named {
                    name: handler.get_ref().clone(),
                    line: lines.line(handler.span()),
                },
                input,
                idempotent: e.idempotent,
            });
            continue;
        }

        // A declared CRUD endpoint.
        let Some(model_name) = &e.model else {
            problems.push(format!(
                "{where_}: an endpoint needs model = \"<app>.<name>\" (CRUD) or a handler"
            ));
            continue;
        };
        for (present, key) in [
            (e.id.is_some(), "id"),
            (e.path.is_some(), "path"),
            (e.permission.is_some(), "permission"),
            (!e.input.is_empty(), "input"),
        ] {
            if present {
                problems.push(format!(
                    "{where_}: {key:?} is for handler endpoints; CRUD endpoints use crud, read, write, filter, order and hooks"
                ));
            }
        }
        let Some(model) = models.iter().find(|m| m.id == *model_name.get_ref()) else {
            problems.push(format!(
                "{where_}: unknown model {:?}; declare it under [[models]]",
                model_name.get_ref()
            ));
            continue;
        };
        if !crud_models.insert(model.id.clone()) {
            problems.push(format!(
                "{where_}: model {:?} already has a CRUD endpoint; list every operation in one crud = [...]",
                model.id
            ));
        }
        let mut ops = BTreeSet::new();
        for op in &e.crud {
            match op.get_ref().as_str() {
                "list" => ops.insert(Crud::List),
                "get" => ops.insert(Crud::Get),
                "create" => ops.insert(Crud::Create),
                "update" => ops.insert(Crud::Update),
                "delete" => ops.insert(Crud::Delete),
                other => {
                    problems.push(format!(
                        "{}: unknown crud operation {other:?}; use list, get, create, update, delete",
                        lines.at(op)
                    ));
                    false
                }
            };
        }
        if ops.is_empty() {
            problems.push(format!("{where_}: crud = [...] lists no operations"));
        }
        let needs_read = ops.iter().any(|o| !o.writes());
        let needs_write = ops.iter().any(|o| o.writes());
        if e.idempotent && !ops.is_empty() && !needs_write {
            problems.push(format!(
                "{where_}: idempotent = true is for writes, and crud = [...] lists only list and get"
            ));
        }
        let read = match &e.read {
            Some(p) => check_permission(p, &mut problems),
            None if needs_read => {
                problems.push(format!(
                    "{where_}: list/get need read = \"<permission>\" (there is no unprotected default)"
                ));
                String::new()
            }
            None => String::new(),
        };
        let write = match &e.write {
            Some(p) => check_permission(p, &mut problems),
            None if needs_write => {
                problems.push(format!(
                    "{where_}: create/update/delete need write = \"<permission>\""
                ));
                String::new()
            }
            None => String::new(),
        };
        let mut filter = Vec::new();
        for f in &e.filter {
            if model
                .field(f.get_ref())
                .is_some_and(|x| x.ty == FieldType::Money)
            {
                problems.push(format!(
                    "{}: money field {:?} cannot be a filter",
                    lines.at(f),
                    f.get_ref()
                ));
            } else if model.field(f.get_ref()).is_some() {
                filter.push(f.get_ref().clone());
            } else {
                problems.push(format!(
                    "{}: filter field {:?} is not a field of {}",
                    lines.at(f),
                    f.get_ref(),
                    model.id
                ));
            }
        }
        let search = e.search.as_ref().map(|s| {
            let field = s.get_ref();
            if !model.field(field).is_some_and(|f| f.ty == FieldType::Text) {
                problems.push(format!("{}: search must name a text field", lines.at(s)));
            }
            if filter.iter().any(|f| f == "q") || model.field("q").is_some() {
                problems.push(format!("{}: search reserves the input q", lines.at(s)));
            }
            field.clone()
        });
        let order = match &e.order {
            None => ("created_at".to_owned(), true),
            Some(o) => {
                let text = o.get_ref();
                let (desc, name) = match text.strip_prefix('-') {
                    Some(n) => (true, n),
                    None => (false, text.as_str()),
                };
                match model.field(name) {
                    None if name != "created_at" => problems.push(format!(
                        "{}: order field {name:?} is not created_at or a field of {}",
                        lines.at(o),
                        model.id
                    )),
                    Some(f) if f.ty == FieldType::Money => problems.push(format!(
                        "{}: order field {name:?} is money; order by its amount in Rust",
                        lines.at(o)
                    )),
                    // Pages continue after the last row's key; a null key
                    // has no place in that order.
                    Some(f) if f.optional => problems.push(format!(
                        "{}: order field {name:?} is optional; order by a required field or created_at",
                        lines.at(o)
                    )),
                    _ => {}
                }
                (name.to_owned(), desc)
            }
        };
        if e.history {
            if !model.history {
                problems.push(format!(
                    "{where_}: history = true publishes the change log of {0}, which needs \
                     history = true on the model {0}",
                    model.id
                ));
            }
            if !ops.contains(&Crud::Get) {
                problems.push(format!(
                    "{where_}: history = true shows the change log of rows the caller can get; \
                     add \"get\" to crud = [...]"
                ));
            }
        }
        let mut hooks = BTreeMap::new();
        for (point, name) in &e.hooks {
            if !HOOKS.contains(&point.as_str()) {
                problems.push(format!(
                    "{}: unknown hook {point:?}; use one of {}",
                    lines.at(name),
                    HOOKS.join(", ")
                ));
                continue;
            }
            hooks.insert(
                point.clone(),
                Named {
                    name: name.get_ref().clone(),
                    line: lines.line(name.span()),
                },
            );
        }
        endpoints.push(EndpointSpec::Crud {
            model: model.id.clone(),
            ops,
            read,
            write,
            filter,
            search,
            order,
            hooks,
            forms: e.forms,
            idempotent: e.idempotent,
            history: e.history,
        });
    }
    // Refs make one app need another (ADR-032), and so does an action's
    // `exists` (ADR-036), so they can't go round in a circle: migrations
    // and lifecycle need an order.
    let mut needs: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let app_of: BTreeMap<&str, &str> = models
        .iter()
        .map(|m| (m.id.as_str(), m.app.as_str()))
        .collect();
    for model in &models {
        for to in model.fields.iter().filter_map(|f| f.to.as_deref()) {
            if let Some(&app) = app_of.get(to)
                && app != model.app
            {
                needs.entry(model.app.as_str()).or_default().insert(app);
            }
        }
    }
    for e in &endpoints {
        let EndpointSpec::Action { id, exists, .. } = e else {
            continue;
        };
        let Some((from, _)) = id.split_once('.') else {
            continue;
        };
        for x in exists {
            if let Some(&app) = app_of.get(x.model.as_str())
                && app != from
            {
                needs.entry(from).or_default().insert(app);
            }
        }
    }
    if let Some(cycle) = app_cycle(&needs) {
        problems.push(format!(
            "{file}: apps refer to each other's models in a circle ({}); refs between apps go one way",
            cycle.join(" → ")
        ));
    }
    check_managed(&lines, &raw.models, &models, &endpoints, &mut problems);
    let admin = admin_specs(
        &lines,
        &raw.admin,
        &models,
        &endpoints,
        &check_permission,
        &mut problems,
    );
    check_forms(file, &models, &endpoints, &admin, &mut problems);
    let mut view_ids = BTreeSet::new();
    for view in &raw.views {
        if !trestle_protocol::valid_stable_id(&view.id)
            || !view_ids.insert(&view.id)
            || !matches!(view.kind.as_str(), "collection" | "record")
            || trestle_protocol::Audience::parse(&view.audience).is_none()
        {
            problems.push(format!("{file}: invalid or duplicate view {}", view.id));
        }
        if view.permission != AUTHENTICATED && !permissions.contains(&view.permission) {
            problems.push(format!(
                "{file}: view {} names an ungranted permission",
                view.id
            ));
        }
        if !models
            .iter()
            .any(|m| view.id.starts_with(&format!("{}.", m.app)))
        {
            problems.push(format!(
                "{file}: view {} needs an installed intent app",
                view.id
            ));
        }
    }

    if let Some(oidc) = &raw.auth.oidc
        && let Err(e) = oidc.provider()
    {
        problems.push(format!("{file}: [auth] {e}"));
    }

    if problems.is_empty() {
        Ok(Intent {
            name: raw.project.name,
            trestle: raw.project.trestle,
            auth: raw.auth,
            tenancy,
            roles,
            scoped,
            models,
            endpoints,
            admin,
            views: raw.views,
        })
    } else {
        Err(IntentError { problems })
    }
}

/// A managed field (ADR-037) takes its default on create, so it needs one
/// (or `optional`), and some action on its model must write it.
fn check_managed(
    lines: &Lines<'_>,
    raw: &[ModelSection],
    models: &[Arc<ModelSpec>],
    endpoints: &[EndpointSpec],
    problems: &mut Vec<String>,
) {
    for model in models {
        let Some(raw_model) = raw.iter().find(|m| *m.id.get_ref() == model.id) else {
            continue;
        };
        for f in model.fields.iter().filter(|f| f.managed()) {
            let at = raw_model
                .fields
                .iter()
                .find(|r| *r.id.get_ref() == f.id)
                .map(|r| lines.at(&r.id))
                .unwrap_or_default();
            if f.required() {
                problems.push(format!(
                    "{at}: field {:?} is written only by actions, so create cannot set it; \
                     make it optional or give it a default",
                    f.id
                ));
            }
            let written = endpoints.iter().any(|e| {
                matches!(e, EndpointSpec::Action { model: m, set, accept, .. }
                    if *m == model.id
                        && (set.iter().any(|(n, _)| *n == f.id) || accept.contains(&f.id)))
            });
            if !written {
                problems.push(format!(
                    "{at}: field {:?} is written only by actions, but no action on {} sets or \
                     accepts it",
                    f.id, model.id
                ));
            }
        }
    }
}

/// A generated form needs a widget for every field it shows: plain `uuid`
/// fields have none (make them `ref`s), and a `ref` picks from its model's
/// list endpoint by that model's label field.
fn check_forms(
    file: &str,
    models: &[Arc<ModelSpec>],
    endpoints: &[EndpointSpec],
    admin: &[AdminSpec],
    problems: &mut Vec<String>,
) {
    let listed = |model: &str| {
        endpoints.iter().any(|e| {
            matches!(e, EndpointSpec::Crud { model: m, ops, .. } if m == model && ops.contains(&Crud::List))
        })
    };
    for e in endpoints {
        let (model, fields, what): (&str, Vec<&str>, String) = match e {
            // Admin views publish create and update forms too.
            EndpointSpec::Crud {
                model, ops, forms, ..
            } if (*forms || admin.iter().any(|a| a.model == *model))
                && (ops.contains(&Crud::Create) || ops.contains(&Crud::Update)) =>
            {
                let Some(m) = models.iter().find(|m| m.id == *model) else {
                    continue;
                };
                (
                    model,
                    m.fields
                        .iter()
                        .filter(|f| !f.managed())
                        .map(|f| f.id.as_str())
                        .collect(),
                    format!("forms of {model}"),
                )
            }
            EndpointSpec::Action {
                id,
                model,
                accept,
                forms: true,
                ..
            } => (
                model,
                accept.iter().map(String::as_str).collect(),
                format!("the form of {id}"),
            ),
            _ => continue,
        };
        let Some(m) = models.iter().find(|m| m.id == *model) else {
            continue;
        };
        for name in fields {
            let Some(f) = m.field(name) else { continue };
            match (f.ty, &f.to) {
                (FieldType::Uuid, _) if !f.optional => problems.push(format!(
                    "{file}: {what} cannot show uuid field {name}; make it a ref (type = \"ref\", to = ...)"
                )),
                (FieldType::Ref, Some(to)) => {
                    let target = models.iter().find(|t| t.id == *to);
                    if !listed(to) || target.is_none_or(|t| t.label.is_none()) {
                        problems.push(format!(
                            "{file}: {what} picks {name} from {to}, which needs a list endpoint and a text label field"
                        ));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Checks the `[[admin]]` sections and resolves their defaults: the
/// table's name as the title, every field as a column, the endpoint's
/// filters.
fn admin_specs(
    lines: &Lines<'_>,
    sections: &[AdminSection],
    models: &[Arc<ModelSpec>],
    endpoints: &[EndpointSpec],
    check_permission: &dyn Fn(&Spanned<String>, &mut Vec<String>) -> String,
    problems: &mut Vec<String>,
) -> Vec<AdminSpec> {
    let mut out: Vec<AdminSpec> = Vec::new();
    for a in sections {
        let at = lines.at(&a.model);
        let id = a.model.get_ref();
        let Some(model) = models.iter().find(|m| m.id == *id) else {
            problems.push(format!(
                "{at}: admin model {id:?} is not a model in this file"
            ));
            continue;
        };
        if out.iter().any(|o| o.model == *id) {
            problems.push(format!("{at}: model {id:?} has two [[admin]] sections"));
        }
        let listed = endpoints.iter().find_map(|e| match e {
            EndpointSpec::Crud {
                model: m,
                ops,
                filter,
                search,
                ..
            } if m == id && ops.contains(&Crud::List) => {
                let mut fields = filter.clone();
                if search.is_some() {
                    fields.push("q".into());
                }
                Some(fields)
            }
            _ => None,
        });
        let Some(endpoint_filters) = listed else {
            problems.push(format!(
                "{at}: the admin view of {id} lists rows, so its CRUD endpoint needs \"list\""
            ));
            continue;
        };
        let Some(permission) = &a.permission else {
            problems.push(format!(
                "{at}: [[admin]] for {id} needs permission = \"<permission>\" (there is no unprotected default)"
            ));
            continue;
        };
        let permission = check_permission(permission, problems);
        let item = |name: &str| {
            ["id", "version", "created_at"].contains(&name) || model.field(name).is_some()
        };
        let columns = match &a.columns {
            None => model.fields.iter().map(|f| f.id.clone()).collect(),
            Some(columns) => {
                for c in columns.iter().filter(|c| !item(c.get_ref())) {
                    problems.push(format!(
                        "{}: admin column {:?} is not id, version, created_at or a field of {id}",
                        lines.at(c),
                        c.get_ref()
                    ));
                }
                columns.iter().map(|c| c.get_ref().clone()).collect()
            }
        };
        let filters = match &a.filters {
            None => endpoint_filters.clone(),
            Some(filters) => {
                for f in filters
                    .iter()
                    .filter(|f| !endpoint_filters.contains(f.get_ref()))
                {
                    problems.push(format!(
                        "{}: admin filter {:?} is not in the filter = [...] of {id}'s CRUD endpoint",
                        lines.at(f),
                        f.get_ref()
                    ));
                }
                filters.iter().map(|f| f.get_ref().clone()).collect()
            }
        };
        let mut actions = Vec::new();
        for action in &a.actions {
            let name = action.get_ref();
            let found = endpoints.iter().any(|e| {
                matches!(e, EndpointSpec::Action { id: i, model: m, .. } if i == name && m == id)
            });
            if !found {
                problems.push(format!(
                    "{}: admin action {name:?} is not an action endpoint on {id}",
                    lines.at(action)
                ));
            } else if name.split_once('.').map(|(app, _)| app) != Some(model.app.as_str()) {
                problems.push(format!(
                    "{}: admin action {name:?} must belong to app {:?}, like {id}",
                    lines.at(action),
                    model.app
                ));
            } else {
                actions.push(name.clone());
            }
        }
        out.push(AdminSpec {
            model: id.clone(),
            permission,
            title: a.title.clone().unwrap_or_else(|| {
                let words = model.table.replace('_', " ");
                let mut chars = words.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            }),
            columns,
            filters,
            actions,
        });
    }
    out
}

/// Checks a declared action and builds it.
fn action_spec(
    lines: &Lines<'_>,
    at: &str,
    (model, models): (&ModelSpec, &[Arc<ModelSpec>]),
    e: &EndpointSection,
    (id, method, path, permission): (&str, &http::Method, &str, &str),
    problems: &mut Vec<String>,
) -> Option<EndpointSpec> {
    let before = problems.len();
    if *method != http::Method::POST || path_params(path) != ["id"] {
        problems.push(format!(
            "{at}: action {id:?} is POST on one row: path = \"POST /api/.../{{id}}/<verb>\""
        ));
    }
    if !e.input.is_empty() {
        problems.push(format!(
            "{at}: action {id:?} takes accept = [\"field\", ...] (fields of {}), not input",
            model.id
        ));
    }
    if e.set.is_empty() && e.accept.is_empty() {
        problems.push(format!(
            "{at}: action {id:?} changes nothing; give it set or accept"
        ));
    }
    let field = |name: &str, problems: &mut Vec<String>| {
        let f = model.field(name);
        if f.is_none() {
            problems.push(format!("{at}: {name:?} is not a field of {}", model.id));
        }
        f
    };
    let mut set = Vec::new();
    for (name, value) in &e.set {
        let Some(f) = field(name, problems) else {
            continue;
        };
        if f.ty == FieldType::Instant && value.as_str() == Some(NOW) {
            set.push((name.clone(), SetValue::Now));
        } else if f.ty != FieldType::Ref && fits(f.ty, &f.values, f.precision, f.scale, value) {
            set.push((
                name.clone(),
                SetValue::Value(serde_json::to_value(value).ok()?),
            ));
        } else {
            problems.push(format!(
                "{at}: action {id:?} sets {name} to {value}, which does not fit the field"
            ));
        }
    }
    let mut when = Vec::new();
    for (name, values) in &e.when {
        let Some(f) = field(name, problems) else {
            continue;
        };
        if values.is_empty()
            || values
                .iter()
                .any(|v| !fits(f.ty, &f.values, f.precision, f.scale, v))
        {
            problems.push(format!(
                "{at}: action {id:?}: when.{name} must list values of the field"
            ));
            continue;
        }
        when.push((
            name.clone(),
            values
                .iter()
                .filter_map(|v| serde_json::to_value(v).ok())
                .collect(),
        ));
    }
    for name in &e.accept {
        if field(name, problems).is_some() && e.set.contains_key(name) {
            problems.push(format!("{at}: action {id:?} both sets and accepts {name}"));
        }
    }
    for name in &e.require {
        field(name, problems);
    }
    let mut check = Vec::new();
    for c in &e.check {
        let Some(f) = field(&c.field, problems) else {
            continue;
        };
        if let Some(spec) = check_spec(at, id, f, c, problems) {
            check.push(spec);
        }
    }
    let mut exists = Vec::new();
    for x in &e.exists {
        if let Some(spec) = exists_spec(at, id, model, models, x, problems) {
            exists.push(spec);
        }
    }
    let repeat = match e.repeat.as_ref().map(|r| (r, r.get_ref().as_str())) {
        None | Some((_, "refuse")) => Repeat::Refuse,
        Some((r, "unchanged")) => {
            if !set.iter().any(|(_, v)| matches!(v, SetValue::Value(_))) {
                problems.push(format!(
                    "{}: action {id:?}: repeat = \"unchanged\" compares the set constants, and it \
                     has none (\"now\" is never unchanged)",
                    lines.at(r)
                ));
            }
            Repeat::Unchanged
        }
        Some((r, other)) => {
            problems.push(format!(
                "{}: repeat = {other:?}; use \"refuse\" (the default) or \"unchanged\"",
                lines.at(r)
            ));
            Repeat::Refuse
        }
    };
    (problems.len() == before).then(|| EndpointSpec::Action {
        id: id.to_owned(),
        model: model.id.clone(),
        path: path.to_owned(),
        permission: permission.to_owned(),
        set,
        when,
        accept: e.accept.clone(),
        require: e.require.clone(),
        check,
        exists,
        repeat,
        forms: e.forms,
        idempotent: e.idempotent,
    })
}

/// One `check` entry: exactly one operator, which suits the field, and a
/// constant that fits it.
fn check_spec(
    at: &str,
    id: &str,
    f: &FieldSpec,
    c: &super::doc::CheckDoc,
    problems: &mut Vec<String>,
) -> Option<CheckSpec> {
    let name = &f.id;
    let given: Vec<(Compare, &toml::Value)> = Compare::ALL
        .into_iter()
        .zip([&c.eq, &c.ne, &c.lt, &c.le, &c.gt, &c.ge])
        .filter_map(|(op, v)| v.as_ref().map(|v| (op, v)))
        .collect();
    let [(op, value)] = given.as_slice() else {
        problems.push(format!(
            "{at}: action {id:?}: a check on {name} takes exactly one of eq, ne, lt, le, gt, ge"
        ));
        return None;
    };
    let op = *op;
    match f.ty {
        FieldType::Ref | FieldType::Uuid => {
            problems.push(format!(
                "{at}: action {id:?}: check cannot compare {name}, an ID; use require or exists"
            ));
            return None;
        }
        FieldType::Bool | FieldType::Text | FieldType::Enum if op.orders() => {
            problems.push(format!(
                "{at}: action {id:?}: check {} on {name}: bool, text and enum fields take eq or ne",
                op.as_str()
            ));
            return None;
        }
        _ => {}
    }
    if !check_fits(f, value) {
        problems.push(format!(
            "{at}: action {id:?}: check {} {value} does not fit {name}",
            op.as_str()
        ));
        return None;
    }
    Some(CheckSpec {
        field: name.clone(),
        op,
        value: serde_json::to_value(value).ok()?,
    })
}

/// A `check` constant: what [`fits`] accepts, plus instants and money (a
/// decimal amount, or `{ amount, currency }` with a currency the field
/// takes).
fn check_fits(f: &FieldSpec, v: &toml::Value) -> bool {
    use trestle_core::protocol::scalar;
    let amount = |s: &str| scalar::check_decimal(s, f.precision, f.scale).is_ok();
    match (f.ty, v) {
        (FieldType::Instant, toml::Value::String(s)) => scalar::normalize_instant(s).is_some(),
        (FieldType::Money, toml::Value::String(s)) => amount(s),
        (FieldType::Money, toml::Value::Table(t)) => {
            t.len() == 2
                && t.get("amount")
                    .and_then(toml::Value::as_str)
                    .is_some_and(amount)
                && t.get("currency")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|c| {
                        scalar::is_currency(c)
                            && (f.currencies.is_empty() || f.currencies.iter().any(|x| x == c))
                    })
        }
        _ => fits(f.ty, &f.values, f.precision, f.scale, v),
    }
}

/// One row scope of `model` (ADR-040): a `uuid` field holding the actor,
/// or a field matched to the `key` of a `through` row that holds it.
fn scope_spec(
    lines: &Lines<'_>,
    (model, models): (&ModelSpec, &[Arc<ModelSpec>]),
    name: &str,
    doc: &Spanned<super::doc::ScopeDoc>,
    tenancy: bool,
    problems: &mut Vec<String>,
) -> Option<ScopeSpec> {
    let at = lines.at(doc);
    let id = &model.id;
    let before = problems.len();
    let doc = doc.get_ref();
    if !ident(name) {
        problems.push(format!(
            "{at}: scope {name:?} of {id} must be a lowercase identifier"
        ));
    }
    if !tenancy {
        problems.push(format!(
            "{at}: scope {name} of {id} needs [tenancy]: scopes limit role grants, which are \
             per tenant membership"
        ));
    }
    let Some(field) = model.field(&doc.field) else {
        problems.push(format!(
            "{at}: scope {name} of {id}: field {:?} is not a field of {id}",
            doc.field
        ));
        return None;
    };
    let Some(through_id) = &doc.through else {
        if doc.key.is_some() || doc.actor.is_some() {
            problems.push(format!(
                "{at}: scope {name} of {id}: key and actor are for a scope with through = \"<model>\""
            ));
        }
        if !doc.conditions.is_empty() {
            problems.push(format!(
                "{at}: scope {name} of {id}: where is for a scope with through = \"<model>\"; \
                 a direct scope reads the actor from the row itself"
            ));
        }
        if field.ty != FieldType::Uuid {
            problems.push(format!(
                "{at}: scope {name} of {id}: without through, field {:?} must be a uuid field \
                 holding the actor",
                field.id
            ));
        }
        return (problems.len() == before).then(|| ScopeSpec {
            field: field.id.clone(),
            through: None,
        });
    };
    let Some(through) = models.iter().find(|m| m.id == *through_id) else {
        problems.push(format!(
            "{at}: scope {name} of {id}: through {through_id:?} is not a model in this file"
        ));
        return None;
    };
    let actor = match &doc.actor {
        None => {
            problems.push(format!(
                "{at}: scope {name} of {id} needs actor = \"<uuid field of {through_id}>\""
            ));
            String::new()
        }
        Some(actor) => {
            if !through
                .field(actor)
                .is_some_and(|f| f.ty == FieldType::Uuid)
            {
                problems.push(format!(
                    "{at}: scope {name} of {id}: actor {actor:?} must be a uuid field of \
                     {through_id} holding the actor"
                ));
            }
            actor.clone()
        }
    };
    let key = doc.key.clone().unwrap_or_else(|| "id".to_owned());
    match (key_type(through, &key), field.ty) {
        (None, _) => problems.push(format!(
            "{at}: scope {name} of {id}: key {key:?} is not id or a field of {through_id}"
        )),
        // A ref to a model that isn't in the file is reported already.
        (_, FieldType::Ref) if field.to_table.is_none() => {}
        (Some(theirs), FieldType::Ref | FieldType::Uuid)
            if theirs == (field.ty, field.to.clone()) => {}
        (Some(_), _) => problems.push(format!(
            "{at}: scope {name} of {id}: field {:?} and {through_id}.{key} are not the same type; \
             a ref must point to the model {key} names, or both be uuid fields",
            field.id
        )),
    }
    let mut conditions = Vec::new();
    for (condition, value) in &doc.conditions {
        let scope = (at.as_str(), name, id.as_str(), through.as_ref());
        if let Some(values) = scope_condition(scope, condition, value, problems) {
            conditions.push((condition.clone(), values));
        }
    }
    (problems.len() == before).then(|| ScopeSpec {
        field: field.id.clone(),
        through: Some(Through {
            model: through.id.clone(),
            table: through.table.clone(),
            key,
            actor,
            conditions,
        }),
    })
}

/// One `where` entry of a `through` scope (ADR-044): a bool, int, text or
/// enum field of the `through` model, and a constant that fits it or a
/// non-empty list of them. The constants become SQL literals, so a NUL
/// character, which no column holds, doesn't fit.
fn scope_condition(
    (at, scope, id, through): (&str, &str, &str, &ModelSpec),
    name: &str,
    value: &toml::Value,
    problems: &mut Vec<String>,
) -> Option<Vec<serde_json::Value>> {
    let model = &through.id;
    let Some(f) = through.field(name) else {
        problems.push(format!(
            "{at}: scope {scope} of {id}: where.{name} is not a field of {model}"
        ));
        return None;
    };
    if !matches!(
        f.ty,
        FieldType::Bool | FieldType::Int | FieldType::Text | FieldType::Enum
    ) {
        problems.push(format!(
            "{at}: scope {scope} of {id}: where.{name} must be a bool, int, text or enum field \
             of {model}"
        ));
        return None;
    }
    let values = match value {
        toml::Value::Array(values) => values.as_slice(),
        one => std::slice::from_ref(one),
    };
    let fit = |v: &toml::Value| {
        fits(f.ty, &f.values, f.precision, f.scale, v)
            && v.as_str().is_none_or(|s| !s.contains('\0'))
    };
    if values.is_empty() || !values.iter().all(fit) {
        problems.push(format!(
            "{at}: scope {scope} of {id}: where.{name} = {value} does not fit {model}.{name}; \
             give a constant of the field or a list of them"
        ));
        return None;
    }
    values
        .iter()
        .map(|v| serde_json::to_value(v).ok())
        .collect()
}

/// The type a `match` side compares as: `id` is a ref to its own model.
fn key_type(model: &ModelSpec, name: &str) -> Option<(FieldType, Option<String>)> {
    if name == "id" {
        return Some((FieldType::Ref, Some(model.id.clone())));
    }
    model.field(name).map(|f| (f.ty, f.to.clone()))
}

/// One `exists` entry: a model of the file, fields on both sides that
/// compare as the same type, and nullable `null` fields.
fn exists_spec(
    at: &str,
    id: &str,
    model: &ModelSpec,
    models: &[Arc<ModelSpec>],
    x: &super::doc::ExistsDoc,
    problems: &mut Vec<String>,
) -> Option<ExistsSpec> {
    let before = problems.len();
    let Some(related) = models.iter().find(|m| m.id == x.model) else {
        problems.push(format!(
            "{at}: action {id:?}: exists names {:?}, which is not a model in this file",
            x.model
        ));
        return None;
    };
    if x.matches.is_empty() {
        problems.push(format!(
            "{at}: action {id:?}: exists {} needs match = {{ <its field> = \"<field of {}>\" }}",
            related.id, model.id
        ));
    }
    for (theirs, ours) in &x.matches {
        let (Some(t), Some(o)) = (key_type(related, theirs), key_type(model, ours)) else {
            problems.push(format!(
                "{at}: action {id:?}: exists {} matches {theirs} = {ours}; {theirs} must be id or a \
                 field of {} and {ours} id or a field of {}",
                related.id, related.id, model.id
            ));
            continue;
        };
        if t.0 == FieldType::Money || t != o {
            problems.push(format!(
                "{at}: action {id:?}: exists {} matches {theirs} = {ours}, which are not the same \
                 type (refs must point to the same model; money cannot be matched)",
                related.id
            ));
        }
    }
    for name in &x.null {
        match related.field(name) {
            None => problems.push(format!(
                "{at}: action {id:?}: exists null {name:?} is not a field of {}",
                related.id
            )),
            Some(f) if !f.optional => problems.push(format!(
                "{at}: action {id:?}: exists null {name:?}: {}.{name} is required, so never null",
                related.id
            )),
            Some(_) => {}
        }
    }
    (problems.len() == before).then(|| ExistsSpec {
        model: related.id.clone(),
        matches: x
            .matches
            .iter()
            .map(|(t, o)| (t.clone(), o.clone()))
            .collect(),
        null: x.null.clone(),
    })
}

fn path_params(path: &str) -> Vec<&str> {
    path.split('/')
        .filter_map(|s| s.strip_prefix('{')?.strip_suffix('}'))
        .collect()
}

/// Whether a TOML constant is a value of this field (defaults, action
/// `set` and `when`).
/// A circle of apps that need each other, starting and ending at the
/// same app, if there is one.
fn app_cycle<'a>(needs: &BTreeMap<&'a str, BTreeSet<&'a str>>) -> Option<Vec<&'a str>> {
    fn visit<'a>(
        app: &'a str,
        needs: &BTreeMap<&'a str, BTreeSet<&'a str>>,
        path: &mut Vec<&'a str>,
        done: &mut BTreeSet<&'a str>,
    ) -> Option<Vec<&'a str>> {
        if let Some(i) = path.iter().position(|a| *a == app) {
            let mut cycle = path[i..].to_vec();
            cycle.push(app);
            return Some(cycle);
        }
        if !done.insert(app) {
            return None;
        }
        path.push(app);
        for next in needs.get(app).into_iter().flatten() {
            if let Some(cycle) = visit(next, needs, path, done) {
                return Some(cycle);
            }
        }
        path.pop();
        None
    }
    let mut done = BTreeSet::new();
    needs
        .keys()
        .find_map(|app| visit(app, needs, &mut Vec::new(), &mut done))
}

fn fits(ty: FieldType, values: &[String], precision: u8, scale: u8, v: &toml::Value) -> bool {
    use trestle_core::protocol::scalar;
    match (ty, v) {
        (FieldType::Bool, toml::Value::Boolean(_)) => true,
        (FieldType::Int, toml::Value::Integer(_)) => true,
        (FieldType::Text, toml::Value::String(_)) => true,
        (FieldType::Enum, toml::Value::String(s)) => values.contains(s),
        (FieldType::Decimal, toml::Value::String(s)) => {
            (1..=38).contains(&precision)
                && scale <= precision
                && scalar::check_decimal(s, precision, scale).is_ok()
        }
        (FieldType::Date, toml::Value::String(s)) => scalar::parse_date(s).is_some(),
        (FieldType::Duration, toml::Value::String(s)) => scalar::parse_duration(s).is_some(),
        _ => false,
    }
}

fn field(lines: &Lines<'_>, f: &FieldSection, problems: &mut Vec<String>) -> Option<FieldSpec> {
    let at = lines.at(&f.id);
    let id = f.id.get_ref();
    if !ident(id) {
        problems.push(format!(
            "{at}: field id {id:?} must be a lowercase identifier"
        ));
        return None;
    }
    if RESERVED.contains(&id.as_str()) {
        problems.push(format!(
            "{at}: field {id:?} is reserved (Trestle adds id, version and created_at)"
        ));
        return None;
    }
    let ty = match f.ty.get_ref().as_str() {
        "text" => FieldType::Text,
        "int" => FieldType::Int,
        "bool" => FieldType::Bool,
        "uuid" => FieldType::Uuid,
        "enum" => FieldType::Enum,
        "decimal" => FieldType::Decimal,
        "date" => FieldType::Date,
        "instant" => FieldType::Instant,
        "duration" => FieldType::Duration,
        "ref" => FieldType::Ref,
        "money" => FieldType::Money,
        other => {
            problems.push(format!(
                "{}: unknown type {other:?}; use text, int, bool, uuid, enum, decimal, money, date, instant, duration or ref",
                lines.at(&f.ty)
            ));
            return None;
        }
    };
    let (precision, scale) = (f.precision.unwrap_or(0), f.scale.unwrap_or(0));
    if ty == FieldType::Money {
        if f.default.is_some() {
            problems.push(format!("{at}: money fields take no default"));
        }
        let codes = &f.currencies;
        if codes
            .iter()
            .enumerate()
            .any(|(i, c)| !trestle_core::protocol::scalar::is_currency(c) || codes[..i].contains(c))
        {
            problems.push(format!(
                "{at}: currencies must be unique three-letter codes, like [\"EUR\", \"USD\"]"
            ));
        }
    } else if !f.currencies.is_empty() {
        problems.push(format!("{at}: currencies = [...] is for money fields"));
    }
    if matches!(ty, FieldType::Decimal | FieldType::Money) {
        if !(1..=38).contains(&precision) || scale > precision {
            problems.push(format!(
                "{at}: a decimal field needs precision = 1..38 and scale <= precision (scale defaults to 0)"
            ));
        }
    } else if f.precision.is_some() || f.scale.is_some() {
        problems.push(format!("{at}: precision and scale are for decimal fields"));
    }
    match (ty, &f.to) {
        (FieldType::Ref, None) => {
            problems.push(format!("{at}: a ref field needs to = \"<model id>\""));
        }
        (FieldType::Ref, Some(_)) => {}
        (_, Some(_)) => problems.push(format!("{at}: to = ... is for ref fields")),
        (_, None) => {}
    }
    if (f.min.is_some() || f.max.is_some()) && !matches!(ty, FieldType::Text | FieldType::Int) {
        problems.push(format!("{at}: min/max apply to text and int fields only"));
    }
    if ty == FieldType::Text && f.min.is_some_and(|m| m < 0) {
        problems.push(format!("{at}: text min must be 0 or more"));
    }
    if ty == FieldType::Enum && f.values.is_empty() {
        problems.push(format!("{at}: an enum field needs values = [\"a\", \"b\"]"));
    }
    if ty != FieldType::Enum && !f.values.is_empty() {
        problems.push(format!("{at}: values = [...] is for enum fields"));
    }
    if ty != FieldType::Enum && !f.labels.is_empty() {
        problems.push(format!("{at}: labels = {{...}} is for enum fields"));
    } else {
        use trestle_core::protocol::contract::MAX_LABEL_CHARS;
        for (value, label) in &f.labels {
            let at = lines.at(label);
            if !f.values.contains(value) {
                problems.push(format!(
                    "{at}: labels.{value}: {value:?} is not one of the field's values"
                ));
            }
            if !(1..=MAX_LABEL_CHARS).contains(&label.get_ref().chars().count()) {
                problems.push(format!(
                    "{at}: labels.{value} must be 1 to {MAX_LABEL_CHARS} characters"
                ));
            }
        }
    }
    let writable = match f.writable.as_ref().map(|w| (w, w.get_ref().as_str())) {
        None | Some((_, "always")) => Writable::Always,
        Some((_, "actions")) => Writable::Actions,
        Some((w, other)) => {
            problems.push(format!(
                "{}: writable = {other:?}; use \"always\" (the default) or \"actions\"",
                lines.at(w)
            ));
            Writable::Always
        }
    };
    let default = match &f.default {
        None => None,
        // The time of the create's transaction, as a database default
        // (ADR-043).
        Some(v) if ty == FieldType::Instant && v.as_str() == Some(NOW) => Some(NOW.into()),
        Some(v) => {
            if !fits(ty, &f.values, precision, scale, v) {
                problems.push(if v.as_str() == Some(NOW) {
                    format!(
                        "{at}: default = \"now\" is the time of the create, for instant fields; \
                         {id:?} is a {} field",
                        f.ty.get_ref()
                    )
                } else if ty == FieldType::Instant {
                    format!(
                        "{at}: default {v} does not fit type \"instant\"; an instant's default \
                         is \"now\", the time of the create"
                    )
                } else {
                    format!("{at}: default {v} does not fit type {:?}", f.ty.get_ref())
                });
            }
            serde_json::to_value(v).ok()
        }
    };
    Some(FieldSpec {
        id: id.clone(),
        ty,
        min: f.min,
        max: f.max,
        optional: f.optional,
        default,
        values: f.values.clone(),
        labels: f
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.get_ref().clone()))
            .collect(),
        precision,
        scale,
        to: f.to.as_ref().map(|t| t.get_ref().clone()),
        to_table: None,
        label: f.label.clone(),
        currencies: f.currencies.clone(),
        writable,
    })
}
