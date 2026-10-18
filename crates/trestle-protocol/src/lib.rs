//! Trestle's portable formats (design doc §3.4, §5, §8, §9).
//!
//! Everything here is plain data plus pure functions: no closures, type IDs,
//! addresses or source paths. One [`ContractDoc`] is the single authority for
//! a wire contract: the server validates with it and every export (JSON
//! Schema, OpenAPI, interaction descriptors) is generated from it (I02).

#![forbid(unsafe_code)]

pub mod contract;
pub mod form;
pub mod interaction;
pub mod scalar;

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub use contract::{Contract, ContractDoc, ContractKind, Field, FieldType, Rejection};
pub use form::{FormPresentation, Widget};

/// Validation semantics every Trestle evaluator implements: required/null
/// rules, Unicode code-point lengths, JSON Schema integer semantics, unknown
/// fields rejected, no trimming or normalization of strings.
pub const VALIDATION_PROFILE: &str = "trestle.portable@1";

/// Provisional reference limits (§8.5). Registry assembly enforces them on
/// the server; renderers enforce the same numbers on what they receive.
pub mod limits {
    /// Serialized size of one interaction descriptor.
    pub const DESCRIPTOR_BYTES: usize = 256 * 1024;
    pub const FORM_FIELDS: usize = 64;
    pub const CONTRACT_FIELDS: usize = 64;
}

/// Who a contract, operation or interaction is published for. Audience is a
/// publication profile, never user authority (§8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Audience {
    Product,
    Admin,
}

impl Audience {
    pub fn as_str(self) -> &'static str {
        match self {
            Audience::Product => "product",
            Audience::Admin => "admin",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "product" => Some(Audience::Product),
            "admin" => Some(Audience::Admin),
            _ => None,
        }
    }
}

/// One field-level error. `pointer` addresses the input instance (RFC 6901).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldError {
    pub pointer: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl FieldError {
    pub fn new(pointer: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        Self {
            pointer: pointer.into(),
            code: code.to_owned(),
            message: message.into(),
            params: None,
        }
    }

    pub fn with_params(mut self, params: Value) -> Self {
        self.params = Some(params);
        self
    }
}

/// JSON Pointer for a top-level property, escaping `~` and `/`.
pub fn pointer(property: &str) -> String {
    format!("/{}", property.replace('~', "~0").replace('/', "~1"))
}

/// Rebuilds `value` with every object's keys in sorted order. Canonical
/// regardless of how `serde_json::Map` is configured in the build; arrays
/// keep their order because order is meaningful there (§3.4).
pub fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonicalize(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// `sha256:<hex>` over the canonical compact encoding. Identifies a
/// representation; it does not prove semantic equivalence.
pub fn digest(value: &Value) -> String {
    let bytes = serde_json::to_vec(&canonicalize(value)).expect("JSON values always serialize");
    let hash = Sha256::digest(bytes);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Canonical, pretty JSON for exports and committed snapshots.
pub fn to_pretty(value: &Value) -> String {
    let mut text =
        serde_json::to_string_pretty(&canonicalize(value)).expect("JSON values always serialize");
    text.push('\n');
    text
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Dotted stable ID such as `lessons.create.input`: at least two segments,
/// the first being the owning app.
pub fn valid_stable_id(value: &str) -> bool {
    let segments: Vec<&str> = value.split('.').collect();
    segments.len() >= 2 && segments.iter().all(|s| valid_identifier(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn digest_ignores_key_order_but_not_array_order() {
        let a = json!({"b": 1, "a": {"y": 2, "x": [1, 2]}});
        let b = json!({"a": {"x": [1, 2], "y": 2}, "b": 1});
        assert_eq!(digest(&a), digest(&b));
        assert_ne!(
            digest(&a),
            digest(&json!({"b": 1, "a": {"y": 2, "x": [2, 1]}}))
        );
        assert!(digest(&a).starts_with("sha256:"));
    }

    #[test]
    fn stable_ids() {
        assert!(valid_stable_id("lessons.create.input"));
        assert!(valid_stable_id("lessons.view"));
        assert!(!valid_stable_id("lessons"));
        assert!(!valid_stable_id("Lessons.view"));
        assert!(!valid_stable_id("lessons..view"));
    }
}
