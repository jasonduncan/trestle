//! Portable, explicitly published application views. Contract schemas are
//! exported from ContractDoc by registry assembly, never inferred from rows.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL: &str = "trestle.views";
pub const VERSION: &str = "1.0";
pub const MAX_BYTES: usize = 256 * 1024;
pub const MAX_COLUMNS: usize = 32;
pub const MAX_NODES: usize = 64;
pub const MAX_DEPTH: usize = 8;
pub const MAX_ROWS: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContractSchema {
    pub id: String,
    pub version: String,
    pub schema: Value,
}

impl From<&crate::ContractDoc> for ContractSchema {
    fn from(contract: &crate::ContractDoc) -> Self {
        Self {
            id: contract.id.clone(),
            version: contract.version.clone(),
            schema: contract.json_schema(),
        }
    }
}

/// Complete checked invocation, including inputless legacy actions. A missing
/// interaction revision means no interaction header is sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationDoc {
    pub operation_id: String,
    pub operation_version: String,
    pub binding_revision: String,
    pub method: String,
    pub path: String,
    pub body: String,
    pub path_params: Vec<String>,
    pub input: ContractSchema,
    pub output: ContractSchema,
    pub success_status: u16,
    pub no_content: bool,
    pub idempotency_key: bool,
    pub operation_version_header: bool,
    pub errors: Vec<u16>,
    pub precondition: Option<PreconditionDoc>,
    pub location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreconditionDoc {
    pub field: String,
    pub etag_of: String,
    pub target: Option<PreconditionReadDoc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreconditionReadDoc {
    pub operation: String,
    pub path: String,
    pub parameter: String,
    pub input_field: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceDoc {
    pub invocation: InvocationDoc,
    pub item: ContractSchema,
    pub items_pointer: Option<String>,
    pub identity_pointer: String,
    pub next_cursor_pointer: Option<String>,
    /// A collection's completeness flag: a required boolean of the output,
    /// `false` when the page stopped before every row (ADR-045).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complete_pointer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ColumnDoc {
    pub id: String,
    pub pointer: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionDoc {
    pub id: String,
    pub label: String,
    pub invocation: InvocationDoc,
    pub interaction_id: Option<String>,
    pub interaction_revision: Option<String>,
    /// Input field -> pointer in the authoritative record.
    pub context: BTreeMap<String, String>,
    pub visible_when: Option<ConditionDoc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConditionDoc {
    pub pointer: String,
    pub equals: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ViewDoc {
    pub protocol: String,
    pub protocol_version: String,
    pub view_id: String,
    pub revision: String,
    pub audience: String,
    pub kind: String,
    pub required_features: Vec<String>,
    pub title: String,
    pub resource: String,
    pub label_pointer: String,
    pub source: SourceDoc,
    pub columns: Vec<ColumnDoc>,
    pub filters: Vec<String>,
    pub record_view: Option<String>,
    pub record_input: BTreeMap<String, String>,
    pub actions: Vec<ActionDoc>,
    pub sections: Vec<SectionDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<AvailabilityDoc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AvailabilityDoc {
    pub invocation: InvocationDoc,
    pub input: BTreeMap<String, String>,
}

/// Application-supplied read operations compute advisory hints from their
/// authorized record. The representation ETag binds hints to that observation.
pub fn availability_contract(id: &str, version: &str) -> crate::ContractDoc {
    use crate::{ContractDoc, Field};
    let action = ContractDoc::output("trestle.view.action_availability", "1")
        .field(Field::text("id").chars(1, 160))
        .field(Field::boolean("available"))
        .field(Field::text("reason").chars(1, 80).nullable());
    ContractDoc::output(id, version)
        .field(Field::text("resource").chars(1, 160))
        .field(Field::text("record_key").chars(1, 160))
        .field(Field::text("etag").chars(1, 256))
        .field(Field::items("actions", &action, 64))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SectionDoc {
    pub id: String,
    pub title: String,
    pub source: SourceDoc,
    pub input: BTreeMap<String, String>,
    pub columns: Vec<ColumnDoc>,
    pub component: Option<String>,
    pub required_component: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererManifest {
    pub protocol: String,
    pub protocol_version: String,
    pub view_versions: Vec<String>,
    pub interaction_versions: Vec<String>,
    pub features: Vec<String>,
    pub components: BTreeMap<String, String>,
    pub digest: String,
}

impl ViewDoc {
    /// Revision excludes only itself; all resolved dependencies participate.
    pub fn seal(&mut self) {
        self.revision.clear();
        let value = serde_json::to_value(&*self).expect("view serializes");
        self.revision = crate::digest(&value);
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.protocol != PROTOCOL || self.protocol_version != VERSION {
            return Err("unsupported view protocol".into());
        }
        if !crate::valid_stable_id(&self.view_id)
            || crate::Audience::parse(&self.audience).is_none()
            || !matches!(self.kind.as_str(), "collection" | "record")
        {
            return Err("invalid view identity, audience or kind".into());
        }
        if self.columns.len() > MAX_COLUMNS
            || self.actions.len()
                + self.columns.len()
                + self
                    .sections
                    .iter()
                    .map(|s| 1 + s.columns.len())
                    .sum::<usize>()
                > MAX_NODES
        {
            return Err("view exceeds presentation limits".into());
        }
        let value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        if serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() > MAX_BYTES {
            return Err("view exceeds byte limit".into());
        }
        safe_json(&value, 0)?;
        let source = &self.source.invocation;
        if source.method != "GET" || source.body != "none" {
            return Err("view source must use a read binding".into());
        }
        for binding in std::iter::once(source).chain(self.actions.iter().map(|a| &a.invocation)) {
            if !safe_path(&binding.path)
                || !matches!(
                    binding.method.as_str(),
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE"
                )
            {
                return Err("invalid view binding".into());
            }
            if !matches!(binding.body.as_str(), "input" | "none")
                || (matches!(binding.method.as_str(), "GET" | "DELETE") != (binding.body == "none"))
                || !(200..300).contains(&binding.success_status)
                || (binding.no_content && binding.success_status != 204)
                || binding
                    .errors
                    .iter()
                    .any(|status| !(400..600).contains(status))
                || binding.location.as_ref().is_some_and(|p| !safe_path(p))
            {
                return Err("invalid invocation semantics".into());
            }
            let params = crate::interaction::HttpBinding::template_params(&binding.path);
            if params != binding.path_params
                || params
                    .iter()
                    .any(|name| binding.input.schema["properties"].get(*name).is_none())
            {
                return Err("invalid path inputs".into());
            }
            for contract in [&binding.input, &binding.output] {
                if !crate::valid_stable_id(&contract.id)
                    || contract.version.is_empty()
                    || contract.schema["type"] != "object"
                    || contract.schema["additionalProperties"] != false
                    || !contract.schema["properties"].is_object()
                    || !contract.schema["required"].is_array()
                {
                    return Err("invalid bound contract".into());
                }
            }
        }
        let expected_item = if self.kind == "collection" {
            &source.output.schema["properties"]["items"]["items"]
        } else {
            &source.output.schema
        };
        let mut item_schema = self.source.item.schema.clone();
        let mut expected_item = expected_item.clone();
        if let Some(map) = item_schema.as_object_mut() {
            map.remove("$schema");
        }
        if let Some(map) = expected_item.as_object_mut() {
            map.remove("$schema");
        }
        if item_schema != expected_item {
            return Err("item contract does not match the published output".into());
        }
        let mut actions = std::collections::BTreeSet::new();
        for action in &self.actions {
            if !actions.insert(&action.id)
                || action.interaction_id.is_some() != action.interaction_revision.is_some()
            {
                return Err("duplicate action or incomplete interaction reference".into());
            }
            if action
                .visible_when
                .as_ref()
                .is_some_and(|c| schema_pointer(&self.source.item.schema, &c.pointer).is_none())
            {
                return Err("invalid availability pointer".into());
            }
            for (field, pointer) in &action.context {
                if !compatible_mapping(
                    schema_pointer(&self.source.item.schema, pointer),
                    action.invocation.input.schema["properties"].get(field),
                ) {
                    return Err("invalid action context mapping".into());
                }
            }
            if action.interaction_id.is_none()
                && action.invocation.input.schema["required"]
                    .as_array()
                    .is_some_and(|fields| {
                        fields.iter().any(|field| {
                            field.as_str().is_none_or(|field| {
                                !action.context.contains_key(field)
                                    && action
                                        .invocation
                                        .precondition
                                        .as_ref()
                                        .is_none_or(|p| p.field != field)
                            })
                        })
                    })
            {
                return Err("action requires an explicit input form".into());
            }
        }
        for pointer in self.columns.iter().map(|c| c.pointer.as_str()).chain([
            self.label_pointer.as_str(),
            self.source.identity_pointer.as_str(),
        ]) {
            if schema_pointer(&self.source.item.schema, pointer).is_none() {
                return Err(format!(
                    "view {}: absent output pointer {pointer}",
                    self.view_id
                ));
            }
        }
        if let Some(pointer) = &self.source.complete_pointer {
            let name = pointer.strip_prefix('/').unwrap_or_default();
            let output = &source.output.schema;
            if self.kind != "collection"
                || output["properties"][name]["type"] != "boolean"
                || !output["required"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|f| f == name))
                || !self
                    .required_features
                    .iter()
                    .any(|f| f == "views.completeness@1")
            {
                return Err("invalid completeness flag".into());
            }
        }
        if self.sections.len() > 16 || (self.kind == "collection" && !self.sections.is_empty()) {
            return Err("invalid record sections".into());
        }
        let mut section_ids = std::collections::BTreeSet::new();
        for section in &self.sections {
            if !section_ids.insert(&section.id) {
                return Err("duplicate section".into());
            }
            let mut nested = self.clone();
            nested.source = section.source.clone();
            nested.columns = section.columns.clone();
            nested.label_pointer = section.source.identity_pointer.clone();
            nested.kind = if section.source.items_pointer.is_some() {
                "collection"
            } else {
                "record"
            }
            .into();
            nested.actions.clear();
            nested.sections.clear();
            nested.availability = None;
            nested.filters.clear();
            nested.validate()?;
            for (field, ptr) in &section.input {
                if !compatible_mapping(
                    schema_pointer(&self.source.item.schema, ptr),
                    section.source.invocation.input.schema["properties"].get(field),
                ) {
                    return Err("invalid section mapping".into());
                }
            }
        }
        if let Some(hints) = &self.availability {
            if self.kind != "record" || hints.invocation.method != "GET" {
                return Err("availability needs a record and a published read".into());
            }
            for (field, ptr) in &hints.input {
                if !compatible_mapping(
                    schema_pointer(&self.source.item.schema, ptr),
                    hints.invocation.input.schema["properties"].get(field),
                ) {
                    return Err("invalid availability mapping".into());
                }
            }
            if hints.invocation.input.schema["required"]
                .as_array()
                .is_some_and(|fields| {
                    fields
                        .iter()
                        .any(|f| f.as_str().is_none_or(|f| !hints.input.contains_key(f)))
                })
            {
                return Err("availability leaves required inputs unbound".into());
            }
        }
        let mut columns = std::collections::BTreeSet::new();
        if self.columns.iter().any(|c| !columns.insert(&c.id)) {
            return Err("duplicate column".into());
        }
        for filter in &self.filters {
            if source
                .input
                .schema
                .get("properties")
                .and_then(|p| p.get(filter))
                .is_none()
            {
                return Err(format!("view {}: unknown filter {filter}", self.view_id));
            }
        }
        Ok(())
    }
}

pub fn schema_pointer<'a>(schema: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut current = schema;
    for segment in pointer.strip_prefix('/')?.split('/') {
        let name = segment.replace("~1", "/").replace("~0", "~");
        current = current.get("properties")?.get(&name)?;
    }
    Some(current)
}

/// A mapping cannot turn an arbitrary display value into a differently typed
/// operation input. Runtime validation still enforces bounds and domain rules.
pub fn compatible_mapping(from: Option<&Value>, to: Option<&Value>) -> bool {
    let (Some(from), Some(to)) = (from, to) else {
        return false;
    };
    let types = |schema: &Value| -> Vec<String> {
        match &schema["type"] {
            Value::String(t) => vec![t.clone()],
            Value::Array(ts) => ts
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => vec![],
        }
    };
    let source = types(from);
    let target = types(to);
    !source.is_empty()
        && source.iter().all(|t| target.contains(t))
        && to
            .get("format")
            .is_none_or(|format| from.get("format") == Some(format))
}

pub fn safe_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains(['\\', '?', '#', '%'])
        && !path.contains("..")
        && !path.chars().any(char::is_whitespace)
}

fn safe_json(value: &Value, depth: usize) -> Result<(), String> {
    // Schema nodes add structural depth beyond the presentation's eight levels.
    if depth > 32 {
        return Err("view exceeds structural depth".into());
    }
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "__proto__"
                        | "prototype"
                        | "constructor"
                        | "$ref"
                        | "$dynamicRef"
                        | "$id"
                        | "$anchor"
                ) {
                    return Err(format!("forbidden view key {key}"));
                }
                safe_json(value, depth + 1)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                safe_json(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
