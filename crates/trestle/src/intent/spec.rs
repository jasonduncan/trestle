//! The `trestle.toml` schema, and its validation into a resolved [`Intent`].
//!
//! The file is data only: no expressions, no conditionals (design §11).
//! Anything conditional is a named Rust handler or hook. Unknown keys are
//! errors, and every problem is reported with its line, all at once.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use serde::Deserialize;
use toml::Spanned;

/// Permission that any signed-in actor holds. Usable without `[tenancy]`.
pub const AUTHENTICATED: &str = "authenticated";

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
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    /// Email and password accounts (default on).
    #[serde(default = "yes")]
    pub passwords: bool,
    /// Opt-in OpenID Connect provider: `"google"`, or a table naming the
    /// issuer and the environment variables that hold the client ID and
    /// secret. Never the values themselves.
    #[serde(default)]
    pub oidc: Option<Oidc>,
}

/// `[auth] oidc`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Oidc {
    /// A well-known provider by name. Only `"google"` today.
    Named(String),
    Custom(OidcProvider),
}

/// `[auth.oidc]` as a table.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
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
    table: Option<String>,
    #[serde(default)]
    fields: Vec<FieldSection>,
    /// Field sets that must be unique (per tenant when [tenancy] is set).
    #[serde(default)]
    unique: Vec<Spanned<Vec<String>>>,
    /// The field that names a row in relation pickers (ADR-026).
    #[serde(default)]
    label: Option<String>,
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
    /// Publish generated forms for this endpoint's writes (ADR-026).
    #[serde(default)]
    forms: bool,
}

/// A validated intent file.
#[derive(Debug, Clone)]
pub struct Intent {
    pub name: String,
    pub trestle: Option<String>,
    pub auth: AuthSection,
    pub tenancy: Option<Tenancy>,
    /// Role → permissions.
    pub roles: BTreeMap<String, BTreeSet<String>>,
    /// Shared with the operations and rows built from them, so the intent
    /// lives exactly as long as something uses it.
    pub models: Vec<Arc<ModelSpec>>,
    pub endpoints: Vec<EndpointSpec>,
    pub admin: Vec<AdminSpec>,
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
    pub default: Option<serde_json::Value>,
    pub values: Vec<String>,
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
}

impl FieldSpec {
    /// Required on create: not optional and no default.
    pub fn required(&self) -> bool {
        !self.optional && self.default.is_none()
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
        /// Column and descending.
        order: (String, bool),
        hooks: BTreeMap<String, Named>,
        forms: bool,
    },
    Custom {
        id: String,
        model: String,
        method: http::Method,
        path: String,
        permission: String,
        handler: Named,
        input: Vec<FieldSpec>,
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
        forms: bool,
    },
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
        Tenancy {
            name: t.model.get_ref().clone(),
        }
    });

    let mut roles = BTreeMap::new();
    let mut permissions = BTreeSet::new();
    for (role, perms) in &raw.roles {
        if !ident(role) {
            problems.push(format!(
                "{file}: role {role:?} must be a lowercase identifier"
            ));
        }
        let set: BTreeSet<String> = perms.iter().map(|p| p.get_ref().clone()).collect();
        permissions.extend(set.iter().cloned());
        roles.insert(role.clone(), set);
    }
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
        let table = m.table.clone().unwrap_or_else(|| format!("{name}s"));
        if !ident(&table) {
            problems.push(format!(
                "{at}: table {table:?} must be a lowercase identifier"
            ));
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
        models.push(Arc::new(ModelSpec {
            id: id.clone(),
            app: app.to_owned(),
            table,
            fields,
            unique,
            label,
        }));
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
        let action = !e.set.is_empty() || !e.when.is_empty() || !e.accept.is_empty();
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
                    &where_,
                    spec,
                    e,
                    (id_text, &method, route, &permission),
                    &mut problems,
                ) {
                    endpoints.push(action);
                }
                continue;
            };
            let mut input = Vec::new();
            for f in &e.input {
                if let Some(spec) = field(&lines, f, &mut problems) {
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
            order,
            hooks,
            forms: e.forms,
        });
    }
    let admin = admin_specs(
        &lines,
        &raw.admin,
        &models,
        &endpoints,
        &check_permission,
        &mut problems,
    );
    check_forms(file, &models, &endpoints, &admin, &mut problems);

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
            models,
            endpoints,
            admin,
        })
    } else {
        Err(IntentError { problems })
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
                    m.fields.iter().map(|f| f.id.as_str()).collect(),
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
                ..
            } if m == id && ops.contains(&Crud::List) => Some(filter),
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
    at: &str,
    model: &ModelSpec,
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
        if f.ty == FieldType::Instant && value.as_str() == Some("now") {
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
    (problems.len() == before).then(|| EndpointSpec::Action {
        id: id.to_owned(),
        model: model.id.clone(),
        path: path.to_owned(),
        permission: permission.to_owned(),
        set,
        when,
        accept: e.accept.clone(),
        forms: e.forms,
    })
}

fn path_params(path: &str) -> Vec<&str> {
    path.split('/')
        .filter_map(|s| s.strip_prefix('{')?.strip_suffix('}'))
        .collect()
}

/// Whether a TOML constant is a value of this field (defaults, action
/// `set` and `when`).
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
    let default = match &f.default {
        None => None,
        Some(v) => {
            let ok = fits(ty, &f.values, precision, scale, v);
            if !ok {
                problems.push(format!(
                    "{at}: default {v} does not fit type {:?}",
                    f.ty.get_ref()
                ));
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
        precision,
        scale,
        to: f.to.as_ref().map(|t| t.get_ref().clone()),
        to_table: None,
        label: f.label.clone(),
        currencies: f.currencies.clone(),
    })
}
