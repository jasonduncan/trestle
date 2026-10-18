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
}

impl FieldType {
    pub fn sql(self) -> &'static str {
        match self {
            FieldType::Text | FieldType::Enum => "text",
            FieldType::Int => "bigint",
            FieldType::Bool => "boolean",
            FieldType::Uuid => "uuid",
            FieldType::Decimal => "numeric",
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
        let mut unique = Vec::new();
        for set in &m.unique {
            let at = lines.at(set);
            let names = set.get_ref();
            if names.is_empty() {
                problems.push(format!("{at}: a unique set needs at least one field"));
                continue;
            }
            for n in names {
                if !fields.iter().any(|f: &FieldSpec| &f.id == n) {
                    problems.push(format!("{at}: unique field {n:?} is not a field of {id}"));
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
        models.push(Arc::new(ModelSpec {
            id: id.clone(),
            app: app.to_owned(),
            table,
            fields,
            unique,
        }));
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
        if let Some(handler) = &e.handler {
            // A custom endpoint written in Rust.
            for (present, key) in [
                (!e.crud.is_empty(), "crud"),
                (e.read.is_some(), "read"),
                (e.write.is_some(), "write"),
                (!e.filter.is_empty(), "filter"),
                (e.order.is_some(), "order"),
                (!e.hooks.is_empty(), "hooks"),
            ] {
                if present {
                    problems.push(format!(
                        "{where_}: {key:?} is for CRUD endpoints; a handler endpoint takes id, path, permission, handler, model and input"
                    ));
                }
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
            if model.field(f.get_ref()).is_some() {
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
                if name != "created_at" && model.field(name).is_none() {
                    problems.push(format!(
                        "{}: order field {name:?} is not created_at or a field of {}",
                        lines.at(o),
                        model.id
                    ));
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
        });
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
            models,
            endpoints,
        })
    } else {
        Err(IntentError { problems })
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
        other => {
            problems.push(format!(
                "{}: unknown type {other:?}; use text, int, bool, uuid, enum, decimal, date, instant or duration",
                lines.at(&f.ty)
            ));
            return None;
        }
    };
    let (precision, scale) = (f.precision.unwrap_or(0), f.scale.unwrap_or(0));
    if ty == FieldType::Decimal {
        if !(1..=38).contains(&precision) || scale > precision {
            problems.push(format!(
                "{at}: a decimal field needs precision = 1..38 and scale <= precision (scale defaults to 0)"
            ));
        }
    } else if f.precision.is_some() || f.scale.is_some() {
        problems.push(format!("{at}: precision and scale are for decimal fields"));
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
            let ok = match (ty, v) {
                (FieldType::Bool, toml::Value::Boolean(_)) => true,
                (FieldType::Int, toml::Value::Integer(_)) => true,
                (FieldType::Text, toml::Value::String(_)) => true,
                (FieldType::Enum, toml::Value::String(s)) => f.values.contains(s),
                (FieldType::Decimal, toml::Value::String(s)) => {
                    trestle_core::protocol::scalar::check_decimal(s, precision, scale).is_ok()
                        && (1..=38).contains(&precision)
                        && scale <= precision
                }
                (FieldType::Date, toml::Value::String(s)) => {
                    trestle_core::protocol::scalar::parse_date(s).is_some()
                }
                (FieldType::Duration, toml::Value::String(s)) => {
                    trestle_core::protocol::scalar::parse_duration(s).is_some()
                }
                _ => false,
            };
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
    })
}
