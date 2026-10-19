//! View assembly and legacy admin lowering. Publication checks stay private.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::FromRef;
use trestle_protocol::interaction::BodySource;
use trestle_protocol::view::*;
use trestle_protocol::{Audience, digest, pointer};

use super::resource::AdminCheck;
use super::{AppBuilder, RegisteredOperation, Registry, page_item};
use crate::identity::Identity;
use crate::invocation::{FromInvocation, Invocation};
use crate::operation::{DynPolicy, Mode, Policy};

pub const UI_MANIFEST_PATH: &str = "/_meta/v1/ui/manifest";
pub const UI_VIEW_PATH: &str = "/_meta/v1/ui/views/{id}";

/// An explicit presentation over registered reads and forms. It adds no API
/// operation, writable input or permission. Default admin views need no builder.
#[derive(Debug, Clone)]
pub struct ViewDefinition {
    pub id: String,
    pub kind: String,
    pub audience: Audience,
    pub resource: String,
    pub query: String,
    pub title: String,
    pub label: String,
    pub identity: String,
    pub columns: Vec<String>,
    pub filters: Vec<String>,
    pub record_view: Option<String>,
    pub record_input: BTreeMap<String, String>,
    pub actions: Vec<String>,
    pub action_input: BTreeMap<String, BTreeMap<String, String>>,
    pub sections: Vec<SectionDefinition>,
    pub availability: Option<(String, BTreeMap<String, String>)>,
}

#[derive(Debug, Clone)]
pub struct SectionDefinition {
    pub id: String,
    pub title: String,
    pub query: String,
    pub collection: bool,
    pub identity: String,
    pub label: String,
    pub columns: Vec<String>,
    pub input: BTreeMap<String, String>,
    pub component: Option<String>,
    pub required_component: bool,
}

/// Application presentation changes cannot add a field or an operation.
#[derive(Debug, Clone, Default)]
pub struct PresentationOverride {
    pub view: String,
    pub title: Option<String>,
    pub columns: Option<Vec<String>>,
    pub labels: BTreeMap<String, String>,
}

impl ViewDefinition {
    pub fn collection(id: &str, query: &str) -> Self {
        Self {
            id: id.into(),
            kind: "collection".into(),
            audience: Audience::Product,
            resource: id.into(),
            query: query.into(),
            title: id.into(),
            label: "id".into(),
            identity: "id".into(),
            columns: vec![],
            filters: vec![],
            record_view: None,
            record_input: BTreeMap::new(),
            actions: vec![],
            action_input: BTreeMap::new(),
            sections: vec![],
            availability: None,
        }
    }
    pub fn record(id: &str, query: &str) -> Self {
        Self {
            kind: "record".into(),
            ..Self::collection(id, query)
        }
    }
    pub fn title(mut self, title: &str) -> Self {
        self.title = title.into();
        self
    }
    pub fn resource(mut self, resource: &str) -> Self {
        self.resource = resource.into();
        self
    }
    pub fn audience(mut self, audience: Audience) -> Self {
        self.audience = audience;
        self
    }
    pub fn label(mut self, field: &str) -> Self {
        self.label = field.into();
        self
    }
    pub fn identity(mut self, field: &str) -> Self {
        self.identity = field.into();
        self
    }
    pub fn columns(mut self, fields: &[&str]) -> Self {
        self.columns = fields.iter().map(|s| (*s).into()).collect();
        self
    }
    pub fn filters(mut self, fields: &[&str]) -> Self {
        self.filters = fields.iter().map(|s| (*s).into()).collect();
        self
    }
    pub fn record_view(mut self, id: &str, input: &str) -> Self {
        self.record_view = Some(id.into());
        self.record_input
            .insert(input.into(), pointer(&self.identity));
        self
    }
    pub fn action(mut self, interaction: &str) -> Self {
        self.actions.push(interaction.into());
        self
    }
    pub fn action_with_context(
        mut self,
        interaction: &str,
        input: BTreeMap<String, String>,
    ) -> Self {
        self.actions.push(interaction.into());
        self.action_input.insert(interaction.into(), input);
        self
    }
    pub fn section(mut self, section: SectionDefinition) -> Self {
        self.sections.push(section);
        self
    }
    pub fn availability(mut self, query: &str, input: BTreeMap<String, String>) -> Self {
        self.availability = Some((query.into(), input));
        self
    }
}

pub(crate) struct ViewRegistration<S> {
    pub definition: ViewDefinition,
    pub check: AdminCheck<S>,
}

pub struct PublishedView<S> {
    pub document: ViewDoc,
    pub(crate) check: AdminCheck<S>,
}

impl<S: Send + Sync + 'static> AppBuilder<S> {
    pub fn presentation_override(&mut self, change: PresentationOverride) -> &mut Self {
        self.view_overrides.push(change);
        self
    }
    pub fn view<C: FromInvocation<S>, P: Policy<C>>(
        &mut self,
        definition: ViewDefinition,
        policy: P,
    ) -> &mut Self
    where
        Identity: FromRef<S>,
    {
        let policy = Arc::new(policy);
        let context = Arc::new(C::describe());
        self.views.push(ViewRegistration {
            definition,
            check: Arc::new(move |parts, state| {
                let policy = policy.clone();
                let context = context.clone();
                Box::pin(async move {
                    let invocation = Invocation::from_http(parts, state, &context).await?;
                    let ctx = C::resolve(&invocation, state).await?;
                    policy.admit(&ctx)
                })
            }),
        });
        self
    }

    pub fn view_dyn(
        &mut self,
        definition: ViewDefinition,
        policy: Arc<dyn DynPolicy<S>>,
    ) -> &mut Self
    where
        Identity: FromRef<S>,
    {
        let context = Arc::new(policy.context());
        self.views.push(ViewRegistration {
            definition,
            check: Arc::new(move |parts, state| {
                let policy = policy.clone();
                let context = context.clone();
                Box::pin(async move {
                    let invocation = Invocation::from_http(parts, state, &context).await?;
                    let ctx = policy.resolve_context(&invocation, state).await?;
                    policy.admit(&ctx)
                })
            }),
        });
        self
    }
}

pub(super) fn invocation<S>(
    op: &RegisteredOperation<S>,
    target: Option<&RegisteredOperation<S>>,
) -> Result<InvocationDoc, String> {
    let b = op
        .binding
        .as_ref()
        .ok_or_else(|| format!("{} has no HTTP binding", op.info.id))?;
    let mut doc = InvocationDoc {
        operation_id: op.info.id.to_string(),
        operation_version: op.info.version.to_string(),
        binding_revision: String::new(),
        method: b.method.clone(),
        path: b.path.clone(),
        body: match b.body {
            BodySource::Input => "input",
            BodySource::None => "none",
        }
        .into(),
        path_params: b.path_params().iter().map(|s| (*s).into()).collect(),
        input: (&*op.input).into(),
        output: (&*op.output).into(),
        success_status: b.success_status,
        no_content: b.no_content,
        idempotency_key: b.idempotency_key,
        operation_version_header: b.operation_version,
        errors: b.errors.clone(),
        precondition: b.if_match.as_ref().map(|field| PreconditionDoc {
            field: field.clone(),
            target: b.precondition_target.as_ref().and_then(|t| {
                target
                    .and_then(|op| op.binding.as_ref())
                    .map(|b| PreconditionReadDoc {
                        operation: t.operation.clone(),
                        path: b.path.clone(),
                        parameter: t.parameter.clone(),
                        input_field: t.input_field.clone(),
                    })
            }),
            etag_of: b
                .precondition_target
                .as_ref()
                .map(|t| t.representation.clone())
                .unwrap_or_else(|| op.output.id.clone()),
        }),
        location: b.location.clone(),
    };
    doc.binding_revision = digest(&serde_json::to_value(&doc).expect("binding serializes"));
    Ok(doc)
}

impl<S> Registry<S> {
    fn view_invocation(&self, op: &RegisteredOperation<S>) -> Result<InvocationDoc, String> {
        let target = op
            .binding
            .as_ref()
            .and_then(|b| b.precondition_target.as_ref())
            .and_then(|t| self.operation(&t.operation));
        invocation(op, target)
    }
    pub fn views(&self) -> &[PublishedView<S>] {
        &self.views
    }
    pub fn views_export(&self, audience: Audience) -> serde_json::Value {
        serde_json::json!({ "protocol": PROTOCOL, "protocolVersion": VERSION,
            "views": self.views.iter().filter(|v| v.document.audience == audience.as_str()).map(|v| &v.document).collect::<Vec<_>>() })
    }

    pub fn check_renderer(&self, renderer: &RendererManifest) -> Vec<String> {
        let mut issues = vec![];
        if renderer.protocol != "trestle.renderer"
            || renderer.protocol_version != "1.0"
            || !renderer.view_versions.iter().any(|v| v == VERSION)
        {
            issues.push("renderer does not support the view protocol".into());
        }
        if renderer.digest.len() != 64 || !renderer.digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            issues.push("renderer digest must identify a compiled build".into());
        }
        for view in &self.views {
            for feature in &view.document.required_features {
                if !renderer.features.contains(feature) {
                    issues.push(format!("view {} requires {feature}", view.document.view_id));
                }
            }
            for section in &view.document.sections {
                if let Some(contract) = section
                    .component
                    .as_ref()
                    .and_then(|key| renderer.components.get(key))
                {
                    let source = &section.source.invocation.output;
                    if contract != &format!("{}@{}", source.id, source.version) {
                        issues.push(format!(
                            "view {} section {} component input contract mismatch",
                            view.document.view_id, section.id
                        ));
                    }
                }

                if section.required_component
                    && section
                        .component
                        .as_ref()
                        .is_none_or(|key| !renderer.components.contains_key(key))
                {
                    issues.push(format!(
                        "view {} section {} requires a missing component",
                        view.document.view_id, section.id
                    ));
                }
            }
        }
        for form in &self.forms {
            if !renderer
                .interaction_versions
                .iter()
                .any(|v| form.descriptor["protocolVersion"] == *v)
            {
                issues.push(format!(
                    "form {} needs an unsupported interaction protocol",
                    form.form.id
                ));
            }
            for feature in form.descriptor["requiredFeatures"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
            {
                if !renderer.features.iter().any(|f| f == feature) {
                    issues.push(format!("form {} requires {feature}", form.form.id));
                }
            }
        }
        issues
    }

    fn describe_view(&self, def: &ViewDefinition, legacy_admin: bool) -> Result<ViewDoc, String> {
        let op = self
            .operation(&def.query)
            .ok_or_else(|| format!("unknown query {}", def.query))?;
        if op.info.mode != Mode::Read
            || (!legacy_admin && !op.info.audiences.contains(&def.audience))
        {
            return Err(format!(
                "{} must be a published read for {}",
                def.query,
                def.audience.as_str()
            ));
        }
        let collection = def.kind == "collection";
        let item = if collection {
            page_item(&op.output).ok_or("collection needs Page<T>")?
        } else {
            &op.output
        };
        let identity = item
            .get(&def.identity)
            .ok_or("missing scalar record identity")?;
        if !matches!(
            identity.ty,
            trestle_protocol::FieldType::Uuid
                | trestle_protocol::FieldType::Text { .. }
                | trestle_protocol::FieldType::Integer { .. }
        ) || identity.nullable
            || !identity.required
        {
            return Err("record identity must be required and scalar".into());
        }
        if collection
            && !matches!(op.output.get("items").map(|f| &f.ty), Some(trestle_protocol::FieldType::Items { max_items, .. }) if *max_items <= MAX_ROWS as u32)
        {
            return Err("collection output must bound its page to 100 rows".into());
        }
        if def.action_input.keys().any(|id| !def.actions.contains(id)) {
            return Err("context mapping names an absent action".into());
        }
        let mut doc = ViewDoc {
            protocol: PROTOCOL.into(),
            protocol_version: VERSION.into(),
            view_id: def.id.clone(),
            revision: String::new(),
            audience: def.audience.as_str().into(),
            kind: def.kind.clone(),
            required_features: vec![format!("views.{}@1", def.kind)],
            title: def.title.clone(),
            resource: def.resource.clone(),
            label_pointer: pointer(&def.label),
            source: SourceDoc {
                invocation: self.view_invocation(op)?,
                item: item.into(),
                items_pointer: collection.then(|| "/items".into()),
                identity_pointer: pointer(&def.identity),
                next_cursor_pointer: collection.then(|| "/next_cursor".into()),
            },
            columns: def
                .columns
                .iter()
                .map(|f| ColumnDoc {
                    id: f.clone(),
                    pointer: pointer(f),
                    label: f.replace('_', " "),
                })
                .collect(),
            filters: def.filters.clone(),
            record_view: def.record_view.clone(),
            record_input: def.record_input.clone(),
            actions: vec![],
            sections: vec![],
            availability: None,
        };
        for id in &def.actions {
            let form = self
                .form(id)
                .ok_or_else(|| format!("unknown interaction {id}"))?;
            if form.form.audience != def.audience {
                return Err(format!("interaction {id} has another audience"));
            }
            let action = self
                .operation(&form.form.operation)
                .ok_or("unknown action")?;
            if action
                .binding
                .as_ref()
                .is_some_and(|b| b.path_params().len() > 1)
                && !def.action_input.contains_key(id)
            {
                return Err(
                    "action needs an explicit context mapping for its path parameters".into(),
                );
            }
            doc.actions.push(ActionDoc {
                id: id.clone(),
                label: form.form.title.clone(),
                invocation: self.view_invocation(action)?,
                interaction_id: Some(id.clone()),
                interaction_revision: Some(form.revision.clone()),
                context: def.action_input.get(id).cloned().unwrap_or_else(|| {
                    action
                        .binding
                        .as_ref()
                        .unwrap()
                        .path_params()
                        .iter()
                        .map(|name| ((*name).into(), pointer(&def.identity)))
                        .collect()
                }),
                visible_when: None,
            });
        }
        if !def.sections.is_empty() {
            if collection || def.sections.len() > 16 {
                return Err("sections require a record view, with at most 16 sections".into());
            }
            doc.required_features.push("views.sections@1".into());
            let mut ids = std::collections::BTreeSet::new();
            for section in &def.sections {
                if !ids.insert(&section.id) {
                    return Err(format!("duplicate section {}", section.id));
                }
                let mut nested = ViewDefinition::collection(
                    &format!("{}.{}", def.id, section.id),
                    &section.query,
                )
                .audience(def.audience);
                nested.kind = if section.collection {
                    "collection"
                } else {
                    "record"
                }
                .into();
                nested.columns = section.columns.clone();
                nested.identity = section.identity.clone();
                nested.label = section.label.clone();
                let source = self.describe_view(&nested, false)?;
                for (field, ptr) in &section.input {
                    if !compatible_mapping(
                        schema_pointer(&doc.source.item.schema, ptr),
                        source.source.invocation.input.schema["properties"].get(field),
                    ) {
                        return Err(format!("invalid section input {}.{field}", section.id));
                    }
                }
                for field in source.source.invocation.input.schema["required"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                {
                    if !section.input.contains_key(field) {
                        return Err(format!("section {} cannot supply {field}", section.id));
                    }
                }
                source.validate()?;
                doc.sections.push(SectionDoc {
                    id: section.id.clone(),
                    title: section.title.clone(),
                    source: source.source,
                    input: section.input.clone(),
                    columns: source.columns,
                    component: section.component.clone(),
                    required_component: section.required_component,
                });
            }
        }
        if let Some((query, input)) = &def.availability {
            let operation = self
                .operation(query)
                .ok_or("unknown availability operation")?;
            if operation.info.mode != Mode::Read
                || !operation.info.audiences.contains(&def.audience)
            {
                return Err("availability must be a published read".into());
            }
            let expected = availability_contract(&operation.output.id, &operation.output.version);
            if operation.output.json_schema() != expected.json_schema() {
                return Err("availability output must use availability_contract".into());
            }
            doc.availability = Some(AvailabilityDoc {
                invocation: self.view_invocation(operation)?,
                input: input.clone(),
            });
            doc.required_features.push("views.availability@1".into());
        }
        Ok(doc)
    }

    pub(crate) fn assemble_views(
        &mut self,
        registrations: Vec<ViewRegistration<S>>,
        overrides: Vec<PresentationOverride>,
    ) -> Result<(), Vec<String>> {
        let mut issues = vec![];
        let mut views = vec![];
        for registration in registrations {
            match self.describe_view(&registration.definition, false) {
                Ok(document) => views.push(PublishedView {
                    document,
                    check: registration.check,
                }),
                Err(error) => issues.push(format!("view {}: {error}", registration.definition.id)),
            }
        }
        for resource in &self.resources {
            let Some(admin) = &resource.admin else {
                continue;
            };
            let Some(list) = &resource.list else { continue };
            let view = &admin.view;
            let collection_id = format!("{}.collection", resource.id);
            let record_id = format!("{}.record", resource.id);
            for (kind, query, id) in
                std::iter::once(("collection", list.as_ref(), collection_id.as_str())).chain(
                    resource
                        .retrieve
                        .as_deref()
                        .map(|q| ("record", q, record_id.as_str())),
                )
            {
                let mut def = ViewDefinition::collection(id, query)
                    .audience(Audience::Admin)
                    .resource(&resource.id)
                    .title(&view.title)
                    .label(&view.label);
                def.kind = kind.into();
                def.columns = view.columns.clone();
                if kind == "collection" {
                    def.filters = view.filters.clone();
                    if resource.retrieve.is_some() {
                        def = def.record_view(&record_id, "id");
                    }
                }
                let result = (|| {
                    // Existing AdminView publication already checked this
                    // binding and its policy. Preserve that legacy audience
                    // behavior; explicit new views use the stricter check.
                    let mut document = self.describe_view(&def, true)?;
                    let forms = [
                        ("create", &view.create_form, &resource.create),
                        ("edit", &view.edit_form, &resource.update),
                    ];
                    for (name, form, op_id) in forms {
                        if let (Some(form), Some(op)) = (
                            form.as_deref().and_then(|f| self.form(f)),
                            op_id.as_deref().and_then(|o| self.operation(o)),
                        ) {
                            document.actions.push(ActionDoc {
                                id: name.into(),
                                label: form.form.title.clone(),
                                invocation: self.view_invocation(op)?,
                                interaction_id: Some(form.form.id.clone()),
                                interaction_revision: Some(form.revision.clone()),
                                context: if name == "edit" {
                                    BTreeMap::from([("id".into(), "/id".into())])
                                } else {
                                    BTreeMap::new()
                                },
                                visible_when: None,
                            });
                        }
                    }
                    let actions = view.actions.iter().filter_map(|a| {
                        resource
                            .actions
                            .iter()
                            .find(|(n, _)| n == &a.name)
                            .map(|(_, id)| (a, id))
                    });
                    for (action, id) in actions {
                        let op = self.operation(id).ok_or("missing action")?;
                        let form = action.form.as_deref().and_then(|f| self.form(f));
                        let mut context = BTreeMap::new();
                        if op.input.get("id").is_some() {
                            context.insert("id".into(), "/id".into());
                        }
                        if op.input.get("expected_version").is_some()
                            && op.binding.as_ref().is_some_and(|b| b.if_match.is_none())
                        {
                            context.insert("expected_version".into(), "/version".into());
                        }
                        document.actions.push(ActionDoc {
                            id: action.name.clone(),
                            label: action.label.clone(),
                            invocation: self.view_invocation(op)?,
                            interaction_id: form.map(|f| f.form.id.clone()),
                            interaction_revision: form.map(|f| f.revision.clone()),
                            context,
                            visible_when: action.visible_when.as_ref().map(|(f, v)| ConditionDoc {
                                pointer: pointer(f),
                                equals: v.clone(),
                            }),
                        });
                    }
                    if let Some(op) = resource.delete.as_deref().and_then(|id| self.operation(id)) {
                        document.actions.push(ActionDoc {
                            id: "delete".into(),
                            label: "Delete".into(),
                            invocation: self.view_invocation(op)?,
                            interaction_id: None,
                            interaction_revision: None,
                            context: BTreeMap::from([("id".into(), "/id".into())]),
                            visible_when: None,
                        });
                    }
                    Ok::<_, String>(document)
                })();
                match result {
                    Ok(document) => views.push(PublishedView {
                        document,
                        check: admin.check.clone(),
                    }),
                    Err(error) => issues.push(format!("view {id}: {error}")),
                }
            }
        }
        views.sort_by(|a, b| a.document.view_id.cmp(&b.document.view_id));
        let mut overridden = std::collections::BTreeSet::new();
        for change in overrides {
            if !overridden.insert(change.view.clone()) {
                issues.push(format!("ambiguous override for {}", change.view));
                continue;
            }
            let Some(view) = views.iter_mut().find(|v| v.document.view_id == change.view) else {
                issues.push(format!("override targets missing view {}", change.view));
                continue;
            };
            if let Some(title) = change.title {
                view.document.title = title;
            }
            for (id, label) in change.labels {
                if let Some(column) = view.document.columns.iter_mut().find(|c| c.id == id) {
                    column.label = label;
                } else {
                    issues.push(format!(
                        "override {} targets missing column {id}",
                        change.view
                    ));
                }
            }
            if let Some(columns) = change.columns {
                let mut seen = std::collections::BTreeSet::new();
                let mut selected = vec![];
                for id in columns {
                    if let Some(column) = view
                        .document
                        .columns
                        .iter()
                        .find(|c| c.id == id)
                        .filter(|_| seen.insert(id.clone()))
                    {
                        selected.push(column.clone());
                    } else {
                        issues.push(format!(
                            "override {} targets missing or duplicate column {id}",
                            change.view
                        ));
                    }
                }
                view.document.columns = selected;
            }
        }
        for pair in views.windows(2) {
            if pair[0].document.view_id == pair[1].document.view_id {
                issues.push(format!("duplicate view {}", pair[0].document.view_id));
            }
        }
        for view in &views {
            if let Some(target) = &view.document.record_view {
                if let Some(record) = views.iter().find(|v| {
                    &v.document.view_id == target
                        && v.document.kind == "record"
                        && v.document.audience == view.document.audience
                }) {
                    let input = &record.document.source.invocation.input.schema;
                    let mappings = &view.document.record_input;
                    for (field, pointer) in mappings {
                        let from = schema_pointer(&view.document.source.item.schema, pointer);
                        let to = input.get("properties").and_then(|p| p.get(field));
                        if !compatible_mapping(from, to) {
                            issues.push(format!(
                                "view {}: incompatible record mapping {field}",
                                view.document.view_id
                            ));
                        }
                    }
                    if input["required"].as_array().is_some_and(|required| {
                        required
                            .iter()
                            .any(|f| f.as_str().is_none_or(|f| !mappings.contains_key(f)))
                    }) {
                        issues.push(format!(
                            "view {}: record mapping leaves required inputs unbound",
                            view.document.view_id
                        ));
                    }
                } else {
                    issues.push(format!(
                        "view {}: missing record view {target}",
                        view.document.view_id
                    ));
                }
            }
        }
        for view in &mut views {
            view.document.seal();
            if let Err(error) = view.document.validate() {
                issues.push(error);
            }
        }
        if !issues.is_empty() {
            return Err(issues);
        }
        self.views = views;
        Ok(())
    }
}
