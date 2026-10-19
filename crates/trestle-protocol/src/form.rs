//! Form presentation: labels, order and widget intent for an operation's
//! input (design doc §9). Presentation never adds fields, validation or
//! authority; it only points at fields the input contract already declares.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::contract::{ContractDoc, FieldType};
use crate::{Audience, limits, valid_stable_id};

/// A logical request for a locally shipped renderer, never a package name
/// or URL (§9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widget {
    Text,
    Textarea,
    Integer,
    Checkbox,
    Select,
    /// Picks a record ID from a scoped list operation (§9.3).
    Relation,
    /// A decimal string (ADR-022): a text box that never turns it into a
    /// float. Needs `forms.decimal@1`.
    Decimal,
    /// A calendar date. Needs `forms.temporal@1`.
    Date,
    /// A date and time, sent with its offset. Needs `forms.temporal@1`.
    DateTime,
    /// A duration such as `PT30M`. Needs `forms.temporal@1`.
    Duration,
    /// An amount and its currency (ADR-027). Needs `forms.money@1`.
    Money,
    /// A variant picker and the chosen variant's fields (ADR-028). Needs
    /// `forms.union@1`.
    Union,
}

impl Widget {
    pub fn key(self) -> &'static str {
        match self {
            Widget::Text => "text",
            Widget::Textarea => "textarea",
            Widget::Integer => "integer",
            Widget::Checkbox => "checkbox",
            Widget::Select => "select",
            Widget::Relation => "relation",
            Widget::Decimal => "decimal",
            Widget::Date => "date",
            Widget::DateTime => "datetime",
            Widget::Duration => "duration",
            Widget::Money => "money",
            Widget::Union => "union",
        }
    }

    /// The renderer feature this widget needs beyond `forms.core@1`.
    pub fn feature(self) -> Option<&'static str> {
        match self {
            Widget::Relation => Some("forms.relation@1"),
            Widget::Decimal => Some("forms.decimal@1"),
            Widget::Date | Widget::DateTime | Widget::Duration => Some("forms.temporal@1"),
            Widget::Money => Some("forms.money@1"),
            Widget::Union => Some("forms.union@1"),
            _ => None,
        }
    }

    pub fn accepts(self, ty: &FieldType) -> bool {
        matches!(
            (self, ty),
            (Widget::Text | Widget::Textarea, FieldType::Text { .. })
                | (Widget::Integer, FieldType::Integer { .. })
                | (Widget::Checkbox, FieldType::Boolean)
                | (Widget::Select, FieldType::Enum { .. })
                | (
                    Widget::Relation,
                    FieldType::Uuid | FieldType::Reference { .. }
                )
                | (Widget::Money, FieldType::Money { .. })
                | (Widget::Union, FieldType::Union { .. })
                | (Widget::Decimal, FieldType::Decimal { .. })
                | (Widget::Date, FieldType::Date)
                | (Widget::DateTime, FieldType::Instant)
                | (Widget::Duration, FieldType::Duration)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField {
    /// JSON Pointer into the operation's input instance, e.g. `/title`.
    pub pointer: String,
    pub widget: Widget,
    pub label: String,
    pub help: Option<String>,
    /// For relation widgets: where options come from.
    pub source: Option<RelationSource>,
}

/// Options for a relation widget come from a list operation, always
/// scoped and authorized by the server; the chosen ID is revalidated on write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationSource {
    /// A list operation whose output has `items` with `id` and `label`.
    pub operation: String,
    /// Item field shown to people.
    pub label: String,
    pub lookup: Option<RelationLookup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationLookup {
    pub resolve_operation: String,
    pub search_field: String,
}

/// Bounded form layout; fields still come exclusively from the input contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    Field(String),
    Section {
        id: String,
        label: String,
        children: Vec<LayoutNode>,
    },
    Columns {
        id: String,
        children: Vec<LayoutNode>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormPresentation {
    pub id: String,
    pub operation: String,
    pub audience: Audience,
    pub title: String,
    pub fields: Vec<FormField>,
    pub submit_label: String,
    pub layout: Option<LayoutNode>,
}

impl FormPresentation {
    pub fn for_operation(id: &str, operation: &str) -> Self {
        Self {
            id: id.to_owned(),
            operation: operation.to_owned(),
            audience: Audience::Product,
            title: String::new(),
            fields: Vec::new(),
            submit_label: "Submit".to_owned(),
            layout: None,
        }
    }

    pub fn audience(mut self, audience: Audience) -> Self {
        self.audience = audience;
        self
    }

    pub fn title(mut self, title: &str) -> Self {
        self.title = title.to_owned();
        self
    }

    pub fn field(mut self, pointer: &str, widget: Widget, label: &str) -> Self {
        self.fields.push(FormField {
            pointer: pointer.to_owned(),
            widget,
            label: label.to_owned(),
            help: None,
            source: None,
        });
        self
    }

    /// A relation picker whose options come from list operation `operation`,
    /// labelled by item field `label_field`.
    pub fn relation(
        mut self,
        pointer: &str,
        label: &str,
        operation: &str,
        label_field: &str,
    ) -> Self {
        self.fields.push(FormField {
            pointer: pointer.to_owned(),
            widget: Widget::Relation,
            label: label.to_owned(),
            help: None,
            source: Some(RelationSource {
                operation: operation.to_owned(),
                label: label_field.to_owned(),
                lookup: None,
            }),
        });
        self
    }

    /// Search and selected-value resolution through two authorized reads.
    pub fn relation_lookup(
        self,
        pointer: &str,
        label: &str,
        search: &str,
        resolve: &str,
        label_field: &str,
        search_field: &str,
    ) -> Self {
        let mut form = self.relation(pointer, label, search, label_field);
        form.fields
            .last_mut()
            .unwrap()
            .source
            .as_mut()
            .unwrap()
            .lookup = Some(RelationLookup {
            resolve_operation: resolve.into(),
            search_field: search_field.into(),
        });
        form
    }

    pub fn layout(mut self, layout: LayoutNode) -> Self {
        self.layout = Some(layout);
        self
    }

    /// Renderer features this form needs beyond the core set.
    /// Renderer features the form's widgets need, sorted: a renderer
    /// without one refuses to submit instead of guessing (I11).
    pub fn features(&self) -> Vec<&'static str> {
        let mut features: Vec<&'static str> = self
            .fields
            .iter()
            .filter_map(|f| f.widget.feature())
            .collect();
        features.sort_unstable();
        if self.layout.is_some() {
            features.push("forms.layout@1");
        }
        if self
            .fields
            .iter()
            .any(|f| f.source.as_ref().is_some_and(|s| s.lookup.is_some()))
        {
            features.push("forms.relation-lookup@1");
        }
        features.dedup();
        features
    }

    /// Help text for the most recently added field.
    pub fn help(mut self, text: &str) -> Self {
        if let Some(field) = self.fields.last_mut() {
            field.help = Some(text.to_owned());
        }
        self
    }

    pub fn submit(mut self, label: &str) -> Self {
        self.submit_label = label.to_owned();
        self
    }

    /// Checks the presentation against the operation's input contract:
    /// every pointer resolves, widgets fit their field types, and every
    /// required field is present unless the binding supplies it (`bound`:
    /// path parameters, `If-Match`), since otherwise the form could never submit.
    pub fn problems(&self, input: &ContractDoc, bound: &[&str]) -> Vec<String> {
        let id = &self.id;
        let mut problems = Vec::new();
        if let Some(layout) = &self.layout {
            fn visit(
                node: &LayoutNode,
                depth: usize,
                nodes: &mut usize,
                fields: &mut Vec<String>,
            ) -> bool {
                *nodes += 1;
                if depth > 8 || *nodes > 64 {
                    return false;
                }
                match node {
                    LayoutNode::Field(pointer) => {
                        fields.push(pointer.clone());
                        true
                    }
                    LayoutNode::Section { children, .. } => {
                        children.iter().all(|n| visit(n, depth + 1, nodes, fields))
                    }
                    LayoutNode::Columns { children, .. } => {
                        !children.is_empty()
                            && children.len() <= 4
                            && children.iter().all(|n| visit(n, depth + 1, nodes, fields))
                    }
                }
            }
            let mut fields = vec![];
            if !visit(layout, 0, &mut 0, &mut fields) {
                problems.push(format!("form {id}: layout exceeds bounds"));
            }
            fields.sort();
            let mut expected: Vec<String> = self.fields.iter().map(|f| f.pointer.clone()).collect();
            expected.sort();
            if fields != expected {
                problems.push(format!(
                    "form {id}: layout must contain every presented field exactly once"
                ));
            }
        }
        if self.fields.len() > limits::FORM_FIELDS {
            problems.push(format!(
                "form {id} has {} fields; the limit is {}",
                self.fields.len(),
                limits::FORM_FIELDS
            ));
        }
        if !valid_stable_id(id) {
            problems.push(format!(
                "form id {id:?} is not a dotted lowercase stable ID"
            ));
        }
        if self.title.is_empty() {
            problems.push(format!("form {id} has no title"));
        }
        for (index, field) in self.fields.iter().enumerate() {
            let pointer = &field.pointer;
            if self.fields[..index].iter().any(|f| f.pointer == *pointer) {
                problems.push(format!("form {id}: {pointer} appears more than once"));
            }
            let Some(name) = pointer.strip_prefix('/').filter(|n| !n.contains('/')) else {
                problems.push(format!(
                    "form {id}: {pointer:?} must point at a top-level input field"
                ));
                continue;
            };
            if (field.widget == Widget::Relation) != field.source.is_some() {
                problems.push(format!(
                    "form {id}: {pointer}: relation widgets need a source, and only they take one"
                ));
            }
            match input.get(name) {
                None => problems.push(format!(
                    "form {id}: {pointer} is not a field of input contract {}",
                    input.id
                )),
                Some(target) if !field.widget.accepts(&target.ty) => problems.push(format!(
                    "form {id}: widget {} cannot edit {} field {pointer}",
                    field.widget.key(),
                    target.ty.name()
                )),
                Some(_) => {}
            }
        }
        for required in input
            .fields
            .iter()
            .filter(|f| f.required && !bound.contains(&f.name.as_str()))
        {
            let pointer = format!("/{}", required.name);
            if !self.fields.iter().any(|f| f.pointer == pointer) {
                problems.push(format!(
                    "form {id}: required input field {pointer} is missing, so the form could never submit"
                ));
            }
        }
        problems
    }

    /// The `presentation` member of an interaction descriptor. `sources`
    /// maps a relation's list operation to its HTTP path.
    pub fn to_json(&self, sources: &BTreeMap<String, String>) -> Value {
        let children: Vec<Value> = self
            .fields
            .iter()
            .map(|f| {
                let mut child = json!({
                    "kind": "field",
                    "pointer": f.pointer,
                    "widget": f.widget.key(),
                    "label": f.label,
                });
                if let Some(help) = &f.help {
                    child["help"] = json!(help);
                }
                if let Some(source) = &f.source {
                    child["options"] = json!({
                        "operation": source.operation,
                        "path": sources.get(&source.operation),
                        "value": "id",
                        "label": source.label,
                    });
                    if let Some(lookup) = &source.lookup {
                        child["options"]["lookup"] = json!({ "resolveOperation": lookup.resolve_operation,
                            "resolvePath": sources.get(&lookup.resolve_operation), "searchField": lookup.search_field });
                    }
                }
                child
            })
            .collect();
        fn render(node: &LayoutNode, fields: &[Value]) -> Value {
            match node {
                LayoutNode::Field(pointer) => fields
                    .iter()
                    .find(|f| f["pointer"] == *pointer)
                    .cloned()
                    .unwrap_or(Value::Null),
                LayoutNode::Section {
                    id,
                    label,
                    children,
                } => {
                    json!({ "kind": "section", "id": id, "label": label, "children": children.iter().map(|n| render(n, fields)).collect::<Vec<_>>() })
                }
                LayoutNode::Columns { id, children } => {
                    json!({ "kind": "columns", "id": id, "children": children.iter().map(|n| render(n, fields)).collect::<Vec<_>>() })
                }
            }
        }
        let layout = self
            .layout
            .as_ref()
            .map(|node| render(node, &children))
            .unwrap_or_else(|| json!({ "kind": "vertical", "children": children }));
        json!({
            "kind": "form",
            "title": self.title,
            "layout": layout,
            "submit": { "label": self.submit_label },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Field;

    fn input() -> ContractDoc {
        ContractDoc::input("lessons.create.input", "1.0.0")
            .field(Field::text("title").chars(1, 200))
            .field(Field::integer("grade_level").range(1, 12))
            .field(Field::text("notes").optional())
    }

    fn form() -> FormPresentation {
        FormPresentation::for_operation("lessons.create.form", "lessons.create")
            .title("Create lesson")
            .field("/title", Widget::Text, "Title")
            .field("/grade_level", Widget::Integer, "Grade level")
            .submit("Create lesson")
    }

    #[test]
    fn valid_form_has_no_problems() {
        assert!(form().problems(&input(), &[]).is_empty());
    }

    #[test]
    fn layouts_cannot_drop_repeat_or_add_editable_fields() {
        let layout = |fields: &[&str]| LayoutNode::Section {
            id: "details".into(),
            label: "Details".into(),
            children: vec![LayoutNode::Columns {
                id: "main".into(),
                children: fields
                    .iter()
                    .map(|field| LayoutNode::Field((*field).into()))
                    .collect(),
            }],
        };
        assert!(
            form()
                .layout(layout(&["/title", "/grade_level"]))
                .problems(&input(), &[])
                .is_empty()
        );
        for fields in [
            vec!["/title"],
            vec!["/title", "/title"],
            vec!["/title", "/school_id"],
        ] {
            assert!(
                form()
                    .layout(layout(&fields))
                    .problems(&input(), &[])
                    .iter()
                    .any(|problem| problem.contains("exactly once"))
            );
        }
    }

    #[test]
    fn unknown_fields_wrong_widgets_and_missing_required_fields_fail() {
        let bad = FormPresentation::for_operation("lessons.create.form", "lessons.create")
            .title("Create lesson")
            .field("/title", Widget::Integer, "Title")
            .field("/school_id", Widget::Text, "School")
            .field("/a/b", Widget::Text, "Nested");
        let problems = bad.problems(&input(), &[]);
        assert_eq!(problems.len(), 4, "{problems:#?}");
        assert!(problems[0].contains("widget integer cannot edit text field /title"));
        assert!(problems[1].contains("/school_id is not a field"));
        assert!(problems[3].contains("/grade_level is missing"));
    }

    #[test]
    fn composite_widgets_fit_only_their_types() {
        let cash = ContractDoc::input("orders.cash", "1");
        let input = ContractDoc::input("orders.pay.input", "1")
            .field(Field::money("total", 10, 2))
            .field(Field::union("payment", "method", &[("cash", &cash)]));
        let form = FormPresentation::for_operation("orders.pay.form", "orders.pay")
            .title("Pay")
            .field("/total", Widget::Money, "Total")
            .field("/payment", Widget::Union, "Payment");
        assert!(
            form.problems(&input, &[]).is_empty(),
            "{:?}",
            form.problems(&input, &[])
        );
        assert_eq!(form.features(), ["forms.money@1", "forms.union@1"]);
        let swapped = FormPresentation::for_operation("orders.pay.form", "orders.pay")
            .title("Pay")
            .field("/total", Widget::Union, "Total")
            .field("/payment", Widget::Money, "Payment");
        assert_eq!(swapped.problems(&input, &[]).len(), 2);
    }

    #[test]
    fn scalar_widgets_fit_their_types_and_name_their_features() {
        let input = ContractDoc::input("orders.create.input", "1")
            .field(Field::decimal("price", 10, 2))
            .field(Field::date("due"))
            .field(Field::instant("at"))
            .field(Field::duration("length"));
        let form = FormPresentation::for_operation("orders.create.form", "orders.create")
            .title("New order")
            .field("/price", Widget::Decimal, "Price")
            .field("/due", Widget::Date, "Due")
            .field("/at", Widget::DateTime, "At")
            .field("/length", Widget::Duration, "Length");
        assert!(
            form.problems(&input, &[]).is_empty(),
            "{:?}",
            form.problems(&input, &[])
        );
        assert_eq!(form.features(), ["forms.decimal@1", "forms.temporal@1"]);

        let wrong = FormPresentation::for_operation("orders.create.form", "orders.create")
            .title("New order")
            .field("/price", Widget::Text, "Price")
            .field("/due", Widget::DateTime, "Due")
            .field("/at", Widget::Date, "At")
            .field("/length", Widget::Integer, "Length");
        let problems = wrong.problems(&input, &[]);
        assert_eq!(problems.len(), 4, "{problems:#?}");
        assert!(problems[0].contains("widget text cannot edit decimal field /price"));
    }

    #[test]
    fn renders_presentation_json() {
        let json = form().help("Shown to students.").to_json(&BTreeMap::new());
        assert_eq!(json["layout"]["children"][0]["pointer"], "/title");
        assert_eq!(json["layout"]["children"][1]["help"], "Shown to students.");
        assert_eq!(json["submit"]["label"], "Create lesson");
    }
}
