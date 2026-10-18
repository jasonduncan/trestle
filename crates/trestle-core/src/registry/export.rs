//! Exports and inspection, all generated from the registry (I02, §11.1).
//! Pure functions of the assembled registry: offline and deterministic.
//!
//! Published exports (contracts, interactions, OpenAPI) are filtered by
//! audience and never include policies, private fields or source paths.
//! Inspection is for operators and may show internal names such as policy IDs.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use trestle_protocol::interaction::BodySource;
use trestle_protocol::{Audience, ContractDoc, digest};

use super::resource::EXPECTED_VERSION_FIELD;
use super::{RegisteredOperation, RegisteredResource, Registry, page_item};

pub const CONTRACTS_FORMAT: &str = "trestle.contracts/1";
pub const INTERACTIONS_FORMAT: &str = "trestle.interactions-export/1";

impl<S> Registry<S> {
    fn published(&self, audience: Audience) -> impl Iterator<Item = &RegisteredOperation<S>> {
        self.operations
            .iter()
            .filter(move |op| op.info.audiences.contains(&audience))
    }

    /// Every contract used by an operation published for `audience`.
    pub fn contracts(&self, audience: Audience) -> BTreeMap<&str, &ContractDoc> {
        self.published(audience)
            .flat_map(|op| [&*op.input, &*op.output])
            .map(|c| (c.id.as_str(), c))
            .collect()
    }

    pub fn contracts_export(&self, audience: Audience) -> Value {
        let contracts: Map<String, Value> = self
            .contracts(audience)
            .into_iter()
            .map(|(id, c)| {
                (
                    id.to_owned(),
                    json!({ "ir": c.to_ir(), "digest": c.digest(), "schema": c.json_schema() }),
                )
            })
            .collect();
        let contracts = Value::Object(contracts);
        json!({
            "format": CONTRACTS_FORMAT,
            "audience": audience.as_str(),
            "digest": digest(&contracts),
            "contracts": contracts,
        })
    }

    pub fn interactions_export(&self, audience: Audience) -> Value {
        let mut descriptors: Vec<Value> = self
            .forms
            .iter()
            .filter(|published| published.form.audience == audience)
            .map(|published| published.descriptor.clone())
            .collect();
        descriptors.sort_by(|a, b| {
            a["interactionId"]
                .as_str()
                .cmp(&b["interactionId"].as_str())
        });
        let descriptors = Value::Array(descriptors);
        json!({
            "format": INTERACTIONS_FORMAT,
            "audience": audience.as_str(),
            "digest": digest(&descriptors),
            "interactions": descriptors,
        })
    }

    /// OpenAPI 3.1.1 with the JSON Schema 2020-12 dialect, for managed routes
    /// published to `audience`. Unmanaged routes are not described: mounting
    /// a handler does not create a contract (§7.3).
    pub fn openapi(&self, audience: Audience) -> Value {
        let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        let mut schemas = Map::new();
        let mut uses_bearer = false;
        for op in self.published(audience) {
            let Some(binding) = &op.binding else { continue };
            let mut parameters = Vec::new();
            for header in &op.context.headers {
                let mut schema = json!({ "type": "string" });
                if let Some(format) = header.format {
                    schema["format"] = json!(format);
                }
                parameters.push(json!({
                    "name": header.name,
                    "in": "header",
                    "required": true,
                    "description": header.description,
                    "schema": schema,
                }));
            }
            let input_schema = op.input.schema_body();
            let path_params = binding.path_params();
            for param in &path_params {
                parameters.push(json!({
                    "name": param,
                    "in": "path",
                    "required": true,
                    "schema": input_schema["properties"][*param],
                }));
            }
            if binding.if_match.is_some() {
                parameters.push(json!({
                    "name": "If-Match",
                    "in": "header",
                    "required": true,
                    "description": format!("ETag of the {} being changed.", op.output.id),
                    "schema": { "type": "string" },
                }));
            }
            if binding.idempotency_key {
                parameters.push(json!({
                    "name": crate::idempotency::IDEMPOTENCY_KEY_HEADER,
                    "in": "header",
                    "required": false,
                    "description": "Repeating a request with the same key and body replays the committed response (header Idempotency-Replayed: true) instead of running it again. Retained for 24 hours; the same key with a different body is 409.",
                    "schema": { "type": "string", "minLength": 1, "maxLength": crate::idempotency::MAX_KEY_CHARS, "pattern": "^[!-~]+$" },
                }));
            }
            // Fields not from the path or a precondition come from the query
            // (GET/DELETE) or the body (everything else).
            let from_request = |name: &str| {
                !path_params.contains(&name) && binding.if_match.as_deref() != Some(name)
            };
            if binding.body == BodySource::None {
                for field in op.input.fields.iter().filter(|f| from_request(&f.name)) {
                    parameters.push(json!({
                        "name": field.name,
                        "in": "query",
                        "required": field.required,
                        "schema": input_schema["properties"][&field.name],
                    }));
                }
            }

            let mut responses = Map::new();
            if binding.no_content {
                responses.insert(
                    binding.success_status.to_string(),
                    json!({ "description": op.info.summary }),
                );
            } else {
                schemas.insert(op.output.id.clone(), op.output.schema_body());
                let mut success = json!({
                    "description": op.info.summary,
                    "content": { "application/json": { "schema": component(&op.output.id) } },
                });
                let mut headers = Map::new();
                if binding.location.is_some() {
                    headers.insert(
                        "Location".into(),
                        json!({ "description": "URL of the created resource.", "schema": { "type": "string" } }),
                    );
                }
                if binding.etag.is_some() {
                    headers.insert(
                        "ETag".into(),
                        json!({ "description": "Send as If-Match to change this version.", "schema": { "type": "string" } }),
                    );
                }
                if !headers.is_empty() {
                    success["headers"] = Value::Object(headers);
                }
                responses.insert(binding.success_status.to_string(), success);
            }
            for status in &binding.errors {
                responses.insert(
                    status.to_string(),
                    json!({ "$ref": "#/components/responses/Problem" }),
                );
            }

            let mut operation = json!({
                "operationId": op.info.id,
                "summary": op.info.summary,
                "tags": [op.app],
                "parameters": parameters,
                "responses": responses,
                "x-trestle-operation-version": op.info.version,
            });
            if binding.body == BodySource::Input {
                let mut body = input_schema.clone();
                let excluded: Vec<String> = op
                    .input
                    .fields
                    .iter()
                    .filter(|f| !from_request(&f.name))
                    .map(|f| f.name.clone())
                    .collect();
                let name = if excluded.is_empty() {
                    op.input.id.clone()
                } else {
                    for field in &excluded {
                        body["properties"].as_object_mut().map(|p| p.remove(field));
                    }
                    if let Some(required) = body["required"].as_array_mut() {
                        required.retain(|r| !excluded.iter().any(|e| r == e.as_str()));
                    }
                    format!("{}.body", op.input.id)
                };
                schemas.insert(name.clone(), body);
                operation["requestBody"] = json!({
                    "required": true,
                    "content": { "application/json": { "schema": component(&name) } },
                });
            }
            if op.context.authenticated {
                uses_bearer = true;
                operation["security"] = json!([{ "bearer": [] }]);
            }
            paths
                .entry(binding.path.clone())
                .or_default()
                .insert(binding.method.to_lowercase(), operation);
        }

        schemas.insert("Problem".into(), problem_schema());
        let mut components = json!({
            "schemas": schemas,
            "responses": {
                "Problem": {
                    "description": "Problem details (RFC 9457) with Trestle's code, request_id and field errors.",
                    "content": { "application/problem+json": { "schema": component("Problem") } },
                }
            },
        });
        if uses_bearer {
            components["securitySchemes"] =
                json!({ "bearer": { "type": "http", "scheme": "bearer" } });
        }
        json!({
            "openapi": "3.1.1",
            "jsonSchemaDialect": "https://json-schema.org/draft/2020-12/schema",
            "info": { "title": self.name, "version": self.version },
            "paths": paths,
            "components": components,
        })
    }

    /// What the admin shell needs to know about one resource: list columns
    /// and filters, the item shape, bindings, forms and actions. Contracts
    /// only; policies stay private.
    pub(crate) fn admin_description(&self, resource: &RegisteredResource<S>) -> Option<Value> {
        let view = resource.admin_view()?;
        let list = self.operation(resource.list.as_deref()?)?;
        let item = page_item(&list.output)?;
        let binding = |id: Option<&str>| {
            let op = self.operation(id?)?;
            let b = op.binding.as_ref()?;
            Some(json!({ "operation": op.info.id, "method": b.method, "path": b.path }))
        };
        let list_input = list.input.schema_body();
        let filters: Vec<Value> = view
            .filters
            .iter()
            .map(|f| json!({ "name": f, "schema": list_input["properties"][f] }))
            .collect();
        let actions: Vec<Value> = view
            .actions
            .iter()
            .filter_map(|action| {
                let (name, label) = (&action.name, &action.label);
                let (_, op_id) = resource.actions.iter().find(|(n, _)| n == name)?;
                let op = self.operation(op_id)?;
                let b = op.binding.as_ref()?;
                let mut bind = serde_json::Map::new();
                if op.input.get(EXPECTED_VERSION_FIELD).is_some() {
                    bind.insert(EXPECTED_VERSION_FIELD.into(), json!("/version"));
                }
                let mut described = json!({
                    "name": name,
                    "label": label,
                    "operation": op.info.id,
                    "path": b.path,
                    "bind": bind,
                });
                if let Some((field, value)) = &action.visible_when {
                    described["visibleWhen"] =
                        json!({ "pointer": format!("/{field}"), "equals": value });
                }
                Some(described)
            })
            .collect();
        let mut retrieve = binding(resource.retrieve.as_deref());
        if let (Some(r), Some(op)) = (
            &mut retrieve,
            resource
                .retrieve
                .as_deref()
                .and_then(|id| self.operation(id)),
        ) {
            r["etagOf"] = json!(op.output.id);
        }
        Some(json!({
            "id": resource.id,
            "title": view.title,
            "label": view.label,
            "path": resource.path,
            "columns": view.columns,
            "filters": filters,
            "list": {
                "operation": list.info.id,
                "path": list.binding.as_ref().map(|b| b.path.clone()),
                "item": item.schema_body(),
            },
            "retrieve": retrieve,
            "update": binding(resource.update.as_deref()).map(|mut b| { b["form"] = json!(view.edit_form); b }),
            "create": binding(resource.create.as_deref()).map(|mut b| { b["form"] = json!(view.create_form); b }),
            "delete": binding(resource.delete.as_deref()),
            "actions": actions,
        }))
    }

    /// Every admin resource description, for review and snapshots.
    pub fn admin_export(&self) -> Value {
        let resources: Vec<Value> = self
            .resources
            .iter()
            .filter_map(|r| self.admin_description(r))
            .collect();
        json!({ "audience": "admin", "digest": digest(&Value::Array(resources.clone())), "resources": resources })
    }

    /// Separate digests for wire contracts and interactions (§3.4), so a
    /// label change is distinguishable from a contract change.
    pub fn fingerprints(&self, audience: Audience) -> Value {
        json!({
            "contracts": self.contracts_export(audience)["digest"],
            "interactions": self.interactions_export(audience)["digest"],
        })
    }

    /// Everything the registry knows about one operation. Internal: includes
    /// the policy ID.
    pub fn inspect_operation(&self, id: &str) -> Option<Value> {
        let op = self.operation(id)?;
        let binding = op.binding.as_ref().map(|b| {
            json!({
                "method": b.method,
                "path": b.path,
                "body": match b.body { BodySource::Input => "input", BodySource::None => "path" },
                "success": b.success_status,
                "location": b.location,
                "errors": b.errors,
            })
        });
        let headers: Vec<&str> = op.context.headers.iter().map(|h| h.name).collect();
        let forms: Vec<Value> = self
            .forms
            .iter()
            .filter(|f| *f.form.operation == *op.info.id)
            .map(|f| {
                let retained: Vec<Value> = f
                    .retained
                    .iter()
                    .map(|r| json!({ "revision": r.revision, "reason": r.reason }))
                    .collect();
                json!({ "id": f.form.id, "revision": f.revision, "retained": retained })
            })
            .collect();
        let contract = |c: &ContractDoc| json!({ "id": c.id, "version": c.version, "digest": c.digest(), "schema": c.json_schema() });
        Some(json!({
            "id": op.info.id,
            "version": op.info.version,
            "app": op.app,
            "summary": op.info.summary,
            "mode": op.info.mode.as_str(),
            "audiences": op.info.audiences.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "policy": op.policy,
            "context": { "authenticated": op.context.authenticated, "headers": headers },
            "input": contract(&op.input),
            "output": contract(&op.output),
            "http": binding,
            "forms": forms,
            "effects": op.info.effects.as_deref(),
        }))
    }
}

fn component(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

fn problem_schema() -> Value {
    json!({
        "type": "object",
        "required": ["type", "title", "status", "code"],
        "properties": {
            "type": { "type": "string" },
            "title": { "type": "string" },
            "status": { "type": "integer" },
            "code": { "type": "string" },
            "detail": { "type": "string" },
            "request_id": { "type": "string" },
            "errors": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["pointer", "code", "message"],
                    "properties": {
                        "pointer": { "type": "string" },
                        "code": { "type": "string" },
                        "message": { "type": "string" },
                        "params": { "type": "object" },
                    },
                },
            },
        },
    })
}
