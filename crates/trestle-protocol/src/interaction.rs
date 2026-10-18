//! Interaction descriptors, protocol `trestle.interactions/1.0` (design doc
//! §8, Appendix B). A descriptor links a wire contract, the operation's HTTP
//! binding and an optional presentation. It never carries policy, secrets
//! or authority: executing the operation always re-authorizes.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::contract::ContractDoc;
use crate::form::FormPresentation;
use crate::{VALIDATION_PROFILE, digest};

pub const PROTOCOL: &str = "trestle.interactions";
/// Pre-release until the M2 conformance fixtures pass.
pub const PROTOCOL_VERSION: &str = "1.0";
pub const REVISION_HEADER: &str = "Trestle-Interaction-Revision";
/// Features every M1 form descriptor needs from a renderer.
pub const CORE_FEATURES: &[&str] = &["forms.core@1", "http.json@1"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySource {
    /// The JSON request body is the input instance.
    Input,
    /// No body; the input instance is built from path parameters.
    None,
}

/// Portable description of how an operation is invoked over HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpBinding {
    pub method: String,
    /// Axum/OpenAPI template syntax, e.g. `/api/v1/lessons/{id}`.
    pub path: String,
    pub body: BodySource,
    pub success_status: u16,
    /// `Location` template filled from output fields, e.g. `/api/v1/lessons/{id}`.
    pub location: Option<String>,
    /// Every status the adapter may return besides success, ascending.
    pub errors: Vec<u16>,
    /// Output field whose value forms the response `ETag`.
    pub etag: Option<String>,
    /// Input field filled from a required `If-Match` precondition.
    pub if_match: Option<String>,
    /// Success has no body (204).
    pub no_content: bool,
    /// Accepts an `Idempotency-Key` header (design doc §6.4).
    pub idempotency_key: bool,
}

/// Header carrying a client's idempotency key.
pub const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
/// How long a receipt answers repeats of its key.
pub const IDEMPOTENCY_RETENTION_SECONDS: u64 = 24 * 60 * 60;

impl HttpBinding {
    /// Names of `{param}` segments in a template.
    pub fn template_params(template: &str) -> Vec<&str> {
        template
            .split('/')
            .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .collect()
    }

    pub fn path_params(&self) -> Vec<&str> {
        Self::template_params(&self.path)
    }
}

pub struct OperationRef<'a> {
    pub id: &'a str,
    pub version: &'a str,
}

/// `{"mode": "none"}` unless the binding accepts an idempotency key. Not a
/// required feature: a client that ignores it still submits correctly.
fn idempotency(binding: &HttpBinding) -> Value {
    if binding.idempotency_key {
        json!({
            "mode": "key",
            "header": IDEMPOTENCY_KEY_HEADER,
            "retentionSeconds": IDEMPOTENCY_RETENTION_SECONDS,
        })
    } else {
        json!({ "mode": "none" })
    }
}

/// Builds the descriptor for a form bound to an operation. The `revision`
/// is derived from everything else in the descriptor, so any change that a
/// renderer could observe produces a new revision.
pub fn form_interaction(
    form: &FormPresentation,
    operation: OperationRef<'_>,
    binding: &HttpBinding,
    input: &ContractDoc,
    output: &ContractDoc,
    sources: &BTreeMap<String, String>,
) -> Value {
    let mut http = json!({
        "method": binding.method,
        "path": binding.path,
        "body": match binding.body { BodySource::Input => "input", BodySource::None => "none" },
        "interactionRevisionHeader": REVISION_HEADER,
    });
    if binding.body == BodySource::Input {
        http["contentType"] = json!("application/json");
    }
    let params = binding.path_params();
    if !params.is_empty() {
        http["pathParams"] = json!(params);
    }
    let mut features: Vec<&str> = CORE_FEATURES.to_vec();
    features.extend(form.features());
    if let Some(field) = &binding.if_match {
        http["precondition"] = json!({ "header": "If-Match", "field": field, "etagOf": output.id });
        features.push("http.precondition@1");
    }
    let mut success = json!({
        "status": binding.success_status,
        "body": if binding.no_content { "none" } else { "output" },
    });
    if let Some(location) = &binding.location {
        success["location"] = json!(location);
    }
    let mut descriptor = json!({
        "protocol": PROTOCOL,
        "protocolVersion": PROTOCOL_VERSION,
        "requiredFeatures": features,
        "interactionId": form.id,
        "audience": form.audience.as_str(),
        "operation": {
            "id": operation.id,
            "version": operation.version,
            "binding": http,
            "success": success,
            "errors": binding.errors,
            "idempotency": idempotency(binding),
            "concurrency": { "mode": "not_applicable" },
        },
        "input": {
            "id": input.id,
            "version": input.version,
            "validationProfile": VALIDATION_PROFILE,
            "schema": input.json_schema(),
        },
        "output": {
            "id": output.id,
            "version": output.version,
            "schema": output.json_schema(),
        },
        "presentation": form.to_json(sources),
    });
    let hash = digest(&descriptor);
    descriptor["revision"] = json!(format!("r-{}", &hash["sha256:".len()..][..16]));
    descriptor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Field;
    use crate::form::Widget;

    #[test]
    fn only_bindings_that_accept_a_key_describe_one() {
        let input = ContractDoc::input("notes.new", "1").field(Field::text("body"));
        let output = ContractDoc::output("notes.view", "1").field(Field::uuid("id"));
        let form = FormPresentation::for_operation("notes.new.form", "notes.add").field(
            "/body",
            Widget::Text,
            "Body",
        );
        let mut binding = HttpBinding {
            method: "POST".into(),
            path: "/notes".into(),
            body: BodySource::Input,
            success_status: 201,
            location: None,
            errors: vec![400],
            etag: None,
            if_match: None,
            no_content: false,
            idempotency_key: false,
        };
        let op = || OperationRef {
            id: "notes.add",
            version: "1",
        };
        let plain = form_interaction(&form, op(), &binding, &input, &output, &BTreeMap::new());
        assert_eq!(plain["operation"]["idempotency"], json!({ "mode": "none" }));
        binding.idempotency_key = true;
        let keyed = form_interaction(&form, op(), &binding, &input, &output, &BTreeMap::new());
        assert_eq!(keyed["operation"]["idempotency"]["mode"], "key");
        assert_eq!(
            keyed["operation"]["idempotency"]["header"],
            "Idempotency-Key"
        );
        assert_eq!(
            keyed["requiredFeatures"], plain["requiredFeatures"],
            "not a required feature: older renderers keep working"
        );
    }

    #[test]
    fn descriptor_follows_specimen_and_revision_tracks_content() {
        let input = ContractDoc::input("lessons.create.input", "1.0.0")
            .field(Field::text("title").chars(1, 200));
        let output = ContractDoc::output("lessons.view", "1.0.0").field(Field::uuid("id"));
        let form = FormPresentation::for_operation("lessons.create.form", "lessons.create")
            .title("Create lesson")
            .field("/title", Widget::Text, "Title");
        let binding = HttpBinding {
            method: "POST".into(),
            path: "/api/v1/lessons".into(),
            body: BodySource::Input,
            success_status: 201,
            location: Some("/api/v1/lessons/{id}".into()),
            errors: vec![400, 422],
            etag: None,
            if_match: None,
            no_content: false,
            idempotency_key: false,
        };
        let op = || OperationRef {
            id: "lessons.create",
            version: "1.0.0",
        };
        let a = form_interaction(&form, op(), &binding, &input, &output, &BTreeMap::new());
        assert_eq!(a["protocol"], "trestle.interactions");
        assert_eq!(a["operation"]["binding"]["contentType"], "application/json");
        assert_eq!(
            a["input"]["schema"]["properties"]["title"]["maxLength"],
            200
        );
        assert_eq!(a["presentation"]["layout"]["children"][0]["widget"], "text");

        let relabeled = form.clone().title("New lesson");
        let b = form_interaction(
            &relabeled,
            op(),
            &binding,
            &input,
            &output,
            &BTreeMap::new(),
        );
        assert_ne!(a["revision"], b["revision"]);
        let again = form_interaction(&form, op(), &binding, &input, &output, &BTreeMap::new());
        assert_eq!(a["revision"], again["revision"]);
    }

    #[test]
    fn template_params() {
        assert_eq!(HttpBinding::template_params("/api/v1/lessons/{id}"), ["id"]);
        assert!(HttpBinding::template_params("/api/v1/lessons").is_empty());
    }
}
