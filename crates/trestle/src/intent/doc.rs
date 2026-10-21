//! The intent as a value (ADR-030): the `trestle.toml` document as plain
//! data, for code that builds an intent instead of writing the file.
//!
//! An [`IntentDoc`] is the file's own schema, key for key. It has no
//! validator of its own: [`IntentDoc::validate`] renders the canonical file
//! ([`IntentDoc::to_toml`]) and runs [`super::parse`] on it, so a document
//! and the file it renders to are accepted, refused and assembled
//! identically. Problem lines refer to that rendering.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use toml::Value;

use super::spec::{AuthSection, Intent, IntentError, parse};

/// A whole intent: `[project]`, `[auth]`, `[tenancy]`, `[roles]`,
/// `[[models]]`, `[[endpoints]]` and `[[admin]]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentDoc {
    pub project: ProjectDoc,
    #[serde(default, skip_serializing_if = "is_default")]
    pub auth: AuthSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenancy: Option<TenancyDoc>,
    /// Role → permissions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admin: Vec<AdminDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<ViewDoc>,
}

/// `[[views]]`: a published presentation over existing registered operations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewDoc {
    pub id: String,
    pub kind: String,
    pub audience: String,
    pub permission: String,
    pub resource: String,
    pub query: String,
    pub title: String,
    #[serde(default = "default_identity")]
    pub identity: String,
    #[serde(default = "default_identity")]
    pub label: String,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub filters: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_view: Option<String>,
    #[serde(default)]
    pub record_input: BTreeMap<String, String>,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub action_input: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub sections: Vec<ViewSectionDoc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSectionDoc {
    pub id: String,
    pub title: String,
    pub query: String,
    #[serde(default)]
    pub collection: bool,
    #[serde(default = "default_identity")]
    pub identity: String,
    #[serde(default = "default_identity")]
    pub label: String,
    pub columns: Vec<String>,
    pub input: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(default)]
    pub required_component: bool,
}

fn default_identity() -> String {
    "id".into()
}

/// `[project]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDoc {
    pub name: String,
    /// The Trestle version the intent was written for (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trestle: Option<String>,
}

/// `[tenancy]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenancyDoc {
    pub model: String,
}

/// `[[models]]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDoc {
    /// Stable ID, `<app>.<model>`.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldDoc>,
    /// Field sets that must be unique (per tenant when `[tenancy]` is set).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unique: Vec<Vec<String>>,
    /// The field that names a row in relation pickers (ADR-026).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A model field, or an input field of a handler endpoint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDoc {
    pub id: String,
    /// `text`, `int`, `bool`, `uuid`, `enum`, `decimal`, `date`, `instant`,
    /// `duration`, `ref` or `money`.
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Enum fields: value → what people see; values without one display as
    /// themselves. Presentation only (ADR-035).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u8>,
    /// For `ref` fields: the model referred to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// What a generated form calls the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Money fields: the currencies accepted (default: any code).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub currencies: Vec<String>,
    /// Model fields: `"always"` (the default, also when absent) or
    /// `"actions"`, written only by domain actions (ADR-037).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writable: Option<String>,
}

/// `[[endpoints]]`: CRUD on a model, a handler endpoint, or a declared
/// action. Which one is decided by the keys present, as in the file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crud: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
    /// Bounded case-insensitive substring search on this published text field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// Hook point → handler name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, String>,
    /// `"<METHOD> <path>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<FieldDoc>,
    /// Actions: field → value written (`"now"` on an instant).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, Value>,
    /// Actions: field → the values it may hold for the action to run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub when: BTreeMap<String, Vec<Value>>,
    /// Actions: fields the caller may supply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept: Vec<String>,
    /// Publish generated forms for this endpoint's writes (ADR-026).
    #[serde(default, skip_serializing_if = "is_default")]
    pub forms: bool,
}

/// `[[admin]]`: a model in the admin shell (ADR-029).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminDoc {
    pub model: String,
    /// Who sees it. Every button still runs its operation's own permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
}

impl IntentDoc {
    /// An empty intent named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            project: ProjectDoc {
                name: name.into(),
                trestle: None,
            },
            ..Self::default()
        }
    }

    /// Reads a document from file text (named `file` in messages). Only the
    /// shape is checked here; [`IntentDoc::validate`] checks the meaning.
    pub fn from_toml(file: &str, text: &str) -> Result<Self, IntentError> {
        toml::from_str(text).map_err(|e| {
            let line = e
                .span()
                .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1)
                .unwrap_or(1);
            IntentError {
                problems: vec![format!("{file}:{line}: {}", e.message())],
            }
        })
    }

    /// The canonical file text for this document.
    pub fn to_toml(&self) -> Result<String, IntentError> {
        toml::to_string(self).map_err(|e| IntentError {
            problems: vec![format!("the intent cannot be written as TOML: {e}")],
        })
    }

    /// Validates the document exactly as [`super::parse`] validates its file
    /// (named `file` in messages, with lines in [`IntentDoc::to_toml`]).
    pub fn validate(&self, file: &str) -> Result<Intent, IntentError> {
        parse(file, &self.to_toml()?)
    }
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}
