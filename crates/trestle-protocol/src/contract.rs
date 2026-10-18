//! Contract IR: one definition of a wire contract (design doc §5).
//!
//! A [`ContractDoc`] is consumed three ways, never duplicated:
//!
//! * [`ContractDoc::validate`]: the server's portable validation;
//! * [`ContractDoc::json_schema`]: JSON Schema 2020-12 for clients and OpenAPI;
//! * [`ContractDoc::to_ir`]: the canonical IR that digests and diffs use.
//!
//! Validation outcomes:
//!
//! * body is not an object → [`Rejection::NotObject`] (HTTP 400)
//! * a field has the wrong JSON type → [`Rejection::Shape`] (HTTP 400)
//! * missing, null, out-of-range, bad format or unknown fields →
//!   [`Rejection::Invalid`] (HTTP 422)
//!
//! Business rules that need the database (uniqueness, membership, workflow
//! state) are never part of a contract (§5.2).

use serde_json::{Map, Number, Value, json};

use crate::{
    FieldError, VALIDATION_PROFILE, digest, limits, pointer, scalar, valid_identifier,
    valid_stable_id,
};

/// A Rust type bound to a registered wire contract. Implementations return
/// a `static` so the definition is built once and shared by every consumer.
pub trait Contract {
    fn contract() -> &'static ContractDoc;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractKind {
    Input,
    Output,
}

impl ContractKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ContractKind::Input => "input",
            ContractKind::Output => "output",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    /// Length in Unicode code points.
    Text {
        min_chars: Option<u32>,
        max_chars: Option<u32>,
    },
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
    Boolean,
    /// RFC 9562 textual form, `8-4-4-4-12` hex.
    Uuid,
    /// Decimal digits as a string: version tokens and 64-bit IDs that must
    /// not pass through JavaScript numbers (§5.4).
    Digits,
    Enum {
        values: Vec<String>,
    },
    /// A bounded list of objects of another contract, embedded inline (no
    /// references). Output contracts only in this profile.
    Items {
        item: Box<ContractDoc>,
        max_items: u32,
    },
    /// A base-10 number as a string (`"-12.50"`), never a JSON number, so
    /// no client rounds it: at most `precision` digits, `scale` of them
    /// after the point (ADR-022).
    Decimal {
        precision: u8,
        scale: u8,
    },
    /// A calendar date, RFC 3339 `full-date` (`2026-10-17`).
    Date,
    /// A point in time, RFC 3339 `date-time` with an offset. Normalized to
    /// UTC (`…Z`).
    Instant,
    /// An exact span, ISO 8601 `P[nD][T[nH][nM][nS]]`: no years or months,
    /// whose length varies.
    Duration,
}

impl FieldType {
    pub fn name(&self) -> &'static str {
        match self {
            FieldType::Items { .. } => "items",
            FieldType::Text { .. } => "text",
            FieldType::Integer { .. } => "integer",
            FieldType::Boolean => "boolean",
            FieldType::Uuid => "uuid",
            FieldType::Digits => "digits",
            FieldType::Enum { .. } => "enum",
            FieldType::Decimal { .. } => "decimal",
            FieldType::Date => "date",
            FieldType::Instant => "instant",
            FieldType::Duration => "duration",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    pub required: bool,
    pub nullable: bool,
    pub description: Option<String>,
    misuse: Vec<&'static str>,
}

impl Field {
    fn new(name: &str, ty: FieldType) -> Self {
        Self {
            name: name.to_owned(),
            ty,
            required: true,
            nullable: false,
            description: None,
            misuse: Vec::new(),
        }
    }

    pub fn text(name: &str) -> Self {
        Self::new(
            name,
            FieldType::Text {
                min_chars: None,
                max_chars: None,
            },
        )
    }

    pub fn integer(name: &str) -> Self {
        Self::new(
            name,
            FieldType::Integer {
                minimum: None,
                maximum: None,
            },
        )
    }

    pub fn boolean(name: &str) -> Self {
        Self::new(name, FieldType::Boolean)
    }

    pub fn uuid(name: &str) -> Self {
        Self::new(name, FieldType::Uuid)
    }

    pub fn digits(name: &str) -> Self {
        Self::new(name, FieldType::Digits)
    }

    /// A decimal string with at most `precision` digits, `scale` after the
    /// point (`decimal("price", 10, 2)` accepts `"99999999.99"`).
    pub fn decimal(name: &str, precision: u8, scale: u8) -> Self {
        Self::new(name, FieldType::Decimal { precision, scale })
    }

    pub fn date(name: &str) -> Self {
        Self::new(name, FieldType::Date)
    }

    pub fn instant(name: &str) -> Self {
        Self::new(name, FieldType::Instant)
    }

    pub fn duration(name: &str) -> Self {
        Self::new(name, FieldType::Duration)
    }

    /// A list of at most `max_items` objects of `item`'s contract.
    pub fn items(name: &str, item: &ContractDoc, max_items: u32) -> Self {
        Self::new(
            name,
            FieldType::Items {
                item: Box::new(item.clone()),
                max_items,
            },
        )
    }

    pub fn enumeration(name: &str, values: &[&str]) -> Self {
        Self::new(
            name,
            FieldType::Enum {
                values: values.iter().map(|v| (*v).to_owned()).collect(),
            },
        )
    }

    /// Text length bounds in code points.
    pub fn chars(mut self, min: u32, max: u32) -> Self {
        match &mut self.ty {
            FieldType::Text {
                min_chars,
                max_chars,
            } => {
                *min_chars = Some(min);
                *max_chars = Some(max);
            }
            _ => self.misuse.push("chars() applies only to text fields"),
        }
        self
    }

    /// Inclusive integer bounds.
    pub fn range(mut self, min: i64, max: i64) -> Self {
        match &mut self.ty {
            FieldType::Integer { minimum, maximum } => {
                *minimum = Some(min);
                *maximum = Some(max);
            }
            _ => self.misuse.push("range() applies only to integer fields"),
        }
        self
    }

    /// May be absent. Absence is left absent: no default is inferred (§5.3).
    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    /// May be present as `null`.
    pub fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    pub fn describe(mut self, text: &str) -> Self {
        self.description = Some(text.to_owned());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractDoc {
    pub id: String,
    pub version: String,
    pub kind: ContractKind,
    /// Declaration order; errors are reported in this order.
    pub fields: Vec<Field>,
}

/// Why an instance does not satisfy a contract.
#[derive(Debug, Clone, PartialEq)]
pub enum Rejection {
    NotObject,
    Shape(Vec<FieldError>),
    Invalid(Vec<FieldError>),
}

impl Rejection {
    pub fn errors(&self) -> &[FieldError] {
        match self {
            Rejection::NotObject => &[],
            Rejection::Shape(errors) | Rejection::Invalid(errors) => errors,
        }
    }
}

impl ContractDoc {
    pub fn input(id: &str, version: &str) -> Self {
        Self::new(id, version, ContractKind::Input)
    }

    pub fn output(id: &str, version: &str) -> Self {
        Self::new(id, version, ContractKind::Output)
    }

    fn new(id: &str, version: &str, kind: ContractKind) -> Self {
        Self {
            id: id.to_owned(),
            version: version.to_owned(),
            kind,
            fields: Vec::new(),
        }
    }

    pub fn field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Definition errors. Registry assembly refuses contracts with any.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let id = &self.id;
        if self.fields.len() > limits::CONTRACT_FIELDS {
            problems.push(format!(
                "contract {id} has {} fields; the limit is {}",
                self.fields.len(),
                limits::CONTRACT_FIELDS
            ));
        }
        if !valid_stable_id(id) {
            problems.push(format!(
                "contract id {id:?} is not a dotted lowercase stable ID"
            ));
        }
        if self.version.is_empty() {
            problems.push(format!("contract {id} has no version"));
        }
        for (index, field) in self.fields.iter().enumerate() {
            let name = &field.name;
            if !valid_identifier(name) {
                problems.push(format!(
                    "contract {id}: field name {name:?} is not a lowercase identifier"
                ));
            }
            if self.fields[..index].iter().any(|f| f.name == *name) {
                problems.push(format!("contract {id}: duplicate field {name}"));
            }
            for misuse in &field.misuse {
                problems.push(format!("contract {id}: field {name}: {misuse}"));
            }
            match &field.ty {
                FieldType::Text {
                    min_chars: Some(min),
                    max_chars: Some(max),
                } if min > max => {
                    problems.push(format!(
                        "contract {id}: field {name}: min chars {min} > max {max}"
                    ));
                }
                FieldType::Integer {
                    minimum: Some(min),
                    maximum: Some(max),
                } if min > max => {
                    problems.push(format!(
                        "contract {id}: field {name}: minimum {min} > maximum {max}"
                    ));
                }
                FieldType::Items { item, max_items } => {
                    if self.kind != ContractKind::Output || item.kind != ContractKind::Output {
                        problems.push(format!(
                            "contract {id}: field {name}: lists are only supported in output contracts"
                        ));
                    }
                    if field.nullable || *max_items == 0 {
                        problems.push(format!(
                            "contract {id}: field {name}: lists must be non-nullable with max_items >= 1"
                        ));
                    }
                    problems.extend(item.problems());
                }
                FieldType::Decimal { precision, scale }
                    if !(1..=38).contains(precision) || scale > precision =>
                {
                    problems.push(format!(
                        "contract {id}: field {name}: decimal needs 1 <= precision <= 38 and scale <= precision"
                    ));
                }
                FieldType::Enum { values } => {
                    if values.is_empty() {
                        problems.push(format!("contract {id}: field {name}: enum has no values"));
                    }
                    for (i, value) in values.iter().enumerate() {
                        if value.is_empty() || values[..i].contains(value) {
                            problems.push(format!(
                                "contract {id}: field {name}: enum values must be non-empty and unique"
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        problems
    }

    /// Validates `value` and returns the normalized instance (only declared
    /// fields; integral floats become integers). Errors follow declaration
    /// order, then unknown fields in sorted order.
    pub fn validate(&self, value: &Value) -> Result<Value, Rejection> {
        let Value::Object(object) = value else {
            return Err(Rejection::NotObject);
        };
        let mut shape = Vec::new();
        let mut invalid = Vec::new();
        let mut normalized = Map::new();

        for field in &self.fields {
            let at = pointer(&field.name);
            match object.get(&field.name) {
                None if field.required => {
                    invalid.push(FieldError::new(at, "required", "This field is required."));
                }
                None => {}
                Some(Value::Null) if field.nullable => {
                    normalized.insert(field.name.clone(), Value::Null);
                }
                Some(Value::Null) => {
                    invalid.push(FieldError::new(
                        at,
                        "not_nullable",
                        "This field cannot be null.",
                    ));
                }
                Some(present) => {
                    if let FieldType::Items { item, max_items } = &field.ty {
                        if let Some(value) =
                            check_items(item, *max_items, &at, present, &mut shape, &mut invalid)
                        {
                            normalized.insert(field.name.clone(), value);
                        }
                        continue;
                    }
                    match check(&field.ty, &at, present) {
                        Check::Ok(value) => {
                            normalized.insert(field.name.clone(), value);
                        }
                        Check::Shape(error) => shape.push(error),
                        Check::Invalid(error) => invalid.push(error),
                    }
                }
            }
        }

        if !shape.is_empty() {
            return Err(Rejection::Shape(shape));
        }
        let mut unknown: Vec<&String> = object
            .keys()
            .filter(|key| self.get(key).is_none())
            .collect();
        unknown.sort();
        for key in unknown {
            invalid.push(FieldError::new(
                pointer(key),
                "unknown_field",
                "This field is not accepted.",
            ));
        }
        if invalid.is_empty() {
            Ok(Value::Object(normalized))
        } else {
            Err(Rejection::Invalid(invalid))
        }
    }

    /// JSON Schema 2020-12 for this contract, self-contained.
    pub fn json_schema(&self) -> Value {
        let mut schema = self.schema_body();
        schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        schema
    }

    /// The schema without `$schema`, for embedding in OpenAPI components.
    pub fn schema_body(&self) -> Value {
        let mut required: Vec<&str> = self
            .fields
            .iter()
            .filter(|f| f.required)
            .map(|f| f.name.as_str())
            .collect();
        required.sort_unstable();
        let properties: Map<String, Value> = self
            .fields
            .iter()
            .map(|f| (f.name.clone(), field_schema(f)))
            .collect();
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": required,
            "properties": properties,
        })
    }

    /// Canonical IR (without digest).
    pub fn to_ir(&self) -> Value {
        let fields: Map<String, Value> = self
            .fields
            .iter()
            .map(|f| {
                let mut ir = json!({
                    "type": f.ty.name(),
                    "required": f.required,
                    "nullable": f.nullable,
                });
                match &f.ty {
                    FieldType::Text {
                        min_chars,
                        max_chars,
                    } => {
                        insert_some(&mut ir, "minChars", min_chars.map(Value::from));
                        insert_some(&mut ir, "maxChars", max_chars.map(Value::from));
                    }
                    FieldType::Integer { minimum, maximum } => {
                        insert_some(&mut ir, "minimum", minimum.map(Value::from));
                        insert_some(&mut ir, "maximum", maximum.map(Value::from));
                    }
                    FieldType::Enum { values } => {
                        ir["values"] = json!(values);
                    }
                    FieldType::Items { item, max_items } => {
                        ir["maxItems"] = json!(max_items);
                        ir["item"] = item.to_ir();
                    }
                    FieldType::Decimal { precision, scale } => {
                        ir["precision"] = json!(precision);
                        ir["scale"] = json!(scale);
                    }
                    FieldType::Boolean
                    | FieldType::Uuid
                    | FieldType::Digits
                    | FieldType::Date
                    | FieldType::Instant
                    | FieldType::Duration => {}
                }
                insert_some(
                    &mut ir,
                    "description",
                    f.description.clone().map(Value::from),
                );
                (f.name.clone(), ir)
            })
            .collect();
        json!({
            "id": self.id,
            "version": self.version,
            "kind": self.kind.as_str(),
            "profile": VALIDATION_PROFILE,
            "fields": fields,
        })
    }

    pub fn digest(&self) -> String {
        digest(&self.to_ir())
    }

    /// Rebuilds a contract from its canonical IR (as exported and committed).
    /// Field order comes back sorted by name; order does not affect meaning.
    pub fn from_ir(ir: &Value) -> Result<Self, String> {
        let text = |key: &str| {
            ir.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("contract IR is missing {key:?}"))
        };
        if ir.get("profile").and_then(Value::as_str) != Some(VALIDATION_PROFILE) {
            return Err(format!("contract IR is not profile {VALIDATION_PROFILE}"));
        }
        let kind = match text("kind")?.as_str() {
            "input" => ContractKind::Input,
            "output" => ContractKind::Output,
            other => return Err(format!("unknown contract kind {other:?}")),
        };
        let fields = ir
            .get("fields")
            .and_then(Value::as_object)
            .ok_or("contract IR has no fields object")?;
        let mut doc = Self::new(&text("id")?, &text("version")?, kind);
        for (name, f) in fields {
            let int = |key: &str| f.get(key).and_then(Value::as_i64);
            let chars = |key: &str| f.get(key).and_then(Value::as_u64).map(|n| n as u32);
            let ty = match f.get("type").and_then(Value::as_str) {
                Some("text") => FieldType::Text {
                    min_chars: chars("minChars"),
                    max_chars: chars("maxChars"),
                },
                Some("integer") => FieldType::Integer {
                    minimum: int("minimum"),
                    maximum: int("maximum"),
                },
                Some("boolean") => FieldType::Boolean,
                Some("items") => FieldType::Items {
                    item: Box::new(Self::from_ir(
                        f.get("item")
                            .ok_or_else(|| format!("list field {name} has no item"))?,
                    )?),
                    max_items: f
                        .get("maxItems")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| format!("list field {name} has no maxItems"))?
                        as u32,
                },
                Some("uuid") => FieldType::Uuid,
                Some("digits") => FieldType::Digits,
                Some("decimal") => {
                    let digits = |key: &str| {
                        f.get(key)
                            .and_then(Value::as_u64)
                            .and_then(|n| u8::try_from(n).ok())
                            .ok_or_else(|| format!("decimal field {name} has no {key}"))
                    };
                    FieldType::Decimal {
                        precision: digits("precision")?,
                        scale: digits("scale")?,
                    }
                }
                Some("date") => FieldType::Date,
                Some("instant") => FieldType::Instant,
                Some("duration") => FieldType::Duration,
                Some("enum") => FieldType::Enum {
                    values: f
                        .get("values")
                        .and_then(Value::as_array)
                        .ok_or_else(|| format!("enum field {name} has no values"))?
                        .iter()
                        .map(|v| v.as_str().map(str::to_owned))
                        .collect::<Option<_>>()
                        .ok_or_else(|| format!("enum field {name} has a non-string value"))?,
                },
                other => return Err(format!("field {name} has unknown type {other:?}")),
            };
            let flag = |key: &str| f.get(key).and_then(Value::as_bool);
            let mut field = Field::new(name, ty);
            field.required = flag("required").ok_or_else(|| format!("field {name}: required?"))?;
            field.nullable = flag("nullable").ok_or_else(|| format!("field {name}: nullable?"))?;
            field.description = f
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned);
            doc.fields.push(field);
        }
        Ok(doc)
    }

    /// Why this contract might reject an instance that `older` accepted.
    /// Empty means every instance valid under `older` is valid here, so a
    /// client built against `older` can keep submitting (§11.3, §11.4).
    pub fn rejects_instances_of(&self, older: &ContractDoc) -> Vec<String> {
        let mut reasons = Vec::new();
        for old in &older.fields {
            let name = &old.name;
            let Some(new) = self.get(name) else {
                reasons.push(format!(
                    "field {name} was removed, so submissions that include it fail"
                ));
                continue;
            };
            if new.required && !old.required {
                reasons.push(format!("field {name} became required"));
            }
            if old.nullable && !new.nullable {
                reasons.push(format!("field {name} no longer accepts null"));
            }
            let widened = |old: Option<i64>, new: Option<i64>, lower: bool| match (old, new) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(o), Some(n)) => {
                    if lower {
                        n <= o
                    } else {
                        n >= o
                    }
                }
            };
            let as_i64 = |v: Option<u32>| v.map(i64::from);
            match (&old.ty, &new.ty) {
                (
                    FieldType::Text {
                        min_chars: omin,
                        max_chars: omax,
                    },
                    FieldType::Text {
                        min_chars: nmin,
                        max_chars: nmax,
                    },
                ) => {
                    if !widened(as_i64(*omin), as_i64(*nmin), true)
                        || !widened(as_i64(*omax), as_i64(*nmax), false)
                    {
                        reasons.push(format!("field {name} has a narrower length range"));
                    }
                }
                (
                    FieldType::Integer {
                        minimum: omin,
                        maximum: omax,
                    },
                    FieldType::Integer {
                        minimum: nmin,
                        maximum: nmax,
                    },
                ) => {
                    if !widened(*omin, *nmin, true) || !widened(*omax, *nmax, false) {
                        reasons.push(format!("field {name} has a narrower numeric range"));
                    }
                }
                (FieldType::Enum { values: ov }, FieldType::Enum { values: nv }) => {
                    if ov.iter().any(|v| !nv.contains(v)) {
                        reasons.push(format!("field {name} dropped enum values"));
                    }
                }
                (
                    FieldType::Decimal {
                        precision: op,
                        scale: os,
                    },
                    FieldType::Decimal {
                        precision: np,
                        scale: ns,
                    },
                ) => {
                    if np - ns < op - os || ns < os {
                        reasons.push(format!("field {name} accepts fewer decimal digits"));
                    }
                }
                (a, b) if a == b => {}
                (a, b) => reasons.push(format!(
                    "field {name} changed type from {} to {}",
                    a.name(),
                    b.name()
                )),
            }
        }
        for new in &self.fields {
            if new.required && older.get(&new.name).is_none() {
                reasons.push(format!("new field {} is required", new.name));
            }
        }
        reasons
    }

    /// A minimal instance that satisfies the contract, with every field
    /// present. Registry checks deserialize it into the bound Rust type to
    /// prove the type and the contract agree.
    pub fn example(&self) -> Value {
        let object: Map<String, Value> = self
            .fields
            .iter()
            .map(|f| {
                let value = match &f.ty {
                    FieldType::Text { min_chars, .. } => {
                        json!("x".repeat(min_chars.unwrap_or(1).max(1) as usize))
                    }
                    FieldType::Integer { minimum, maximum } => {
                        json!(minimum.unwrap_or(0).min(maximum.unwrap_or(i64::MAX)))
                    }
                    FieldType::Boolean => json!(false),
                    FieldType::Uuid => json!("00000000-0000-4000-8000-000000000000"),
                    FieldType::Digits => json!("1"),
                    FieldType::Enum { values } => json!(values.first()),
                    FieldType::Items { item, .. } => json!([item.example()]),
                    FieldType::Decimal { .. } => json!("0"),
                    FieldType::Date => json!("2026-01-01"),
                    FieldType::Instant => json!("2026-01-01T00:00:00Z"),
                    FieldType::Duration => json!("PT1S"),
                };
                (f.name.clone(), value)
            })
            .collect();
        Value::Object(object)
    }
}

fn insert_some(target: &mut Value, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        target[key] = value;
    }
}

fn field_schema(field: &Field) -> Value {
    let mut schema = match &field.ty {
        FieldType::Text {
            min_chars,
            max_chars,
        } => {
            let mut s = json!({ "type": "string" });
            insert_some(&mut s, "minLength", min_chars.map(Value::from));
            insert_some(&mut s, "maxLength", max_chars.map(Value::from));
            s
        }
        FieldType::Integer { minimum, maximum } => {
            let mut s = json!({ "type": "integer" });
            insert_some(&mut s, "minimum", minimum.map(Value::from));
            insert_some(&mut s, "maximum", maximum.map(Value::from));
            s
        }
        FieldType::Boolean => json!({ "type": "boolean" }),
        FieldType::Uuid => json!({ "type": "string", "format": "uuid" }),
        FieldType::Digits => json!({ "type": "string", "pattern": "^[0-9]+$" }),
        FieldType::Enum { values } => json!({ "type": "string", "enum": values }),
        FieldType::Items { item, max_items } => {
            json!({ "type": "array", "maxItems": max_items, "items": item.schema_body() })
        }
        FieldType::Decimal { precision, scale } => {
            json!({ "type": "string", "pattern": scalar::decimal_pattern(*precision, *scale) })
        }
        FieldType::Date => json!({ "type": "string", "format": "date" }),
        FieldType::Instant => json!({ "type": "string", "format": "date-time" }),
        FieldType::Duration => {
            json!({ "type": "string", "format": "duration", "pattern": scalar::DURATION_PATTERN })
        }
    };
    if field.nullable {
        let base = schema["type"].clone();
        schema["type"] = json!([base, "null"]);
        if let Some(values) = schema.get_mut("enum").and_then(Value::as_array_mut) {
            values.push(Value::Null);
        }
    }
    insert_some(
        &mut schema,
        "description",
        field.description.clone().map(Value::from),
    );
    schema
}

enum Check {
    Ok(Value),
    Shape(FieldError),
    Invalid(FieldError),
}

fn check(ty: &FieldType, at: &str, value: &Value) -> Check {
    match ty {
        FieldType::Text {
            min_chars,
            max_chars,
        } => {
            let Value::String(text) = value else {
                return Check::Shape(type_error(at, "string"));
            };
            let count = text.chars().count();
            if let Some(min) = min_chars.map(|m| m as usize)
                && count < min
            {
                let message = if min == 1 {
                    "This field cannot be empty.".to_owned()
                } else {
                    format!("Use at least {min} characters.")
                };
                return Check::Invalid(
                    FieldError::new(at, "min_length", message).with_params(json!({ "min": min })),
                );
            }
            if let Some(max) = max_chars.map(|m| m as usize)
                && count > max
            {
                return Check::Invalid(
                    FieldError::new(at, "max_length", format!("Use {max} characters or fewer."))
                        .with_params(json!({ "max": max })),
                );
            }
            Check::Ok(value.clone())
        }
        FieldType::Integer { minimum, maximum } => {
            let Value::Number(number) = value else {
                return Check::Shape(type_error(at, "integer"));
            };
            let too_small = |min: i64| {
                Check::Invalid(
                    FieldError::new(at, "minimum", format!("Must be at least {min}."))
                        .with_params(json!({ "min": min })),
                )
            };
            let too_large = |max: i64| {
                Check::Invalid(
                    FieldError::new(at, "maximum", format!("Must be at most {max}."))
                        .with_params(json!({ "max": max })),
                )
            };
            let integer = match classify(number) {
                Integer::Fractional => return Check::Shape(type_error(at, "integer")),
                Integer::BelowI64 => return too_small(minimum.unwrap_or(i64::MIN)),
                Integer::AboveI64 => return too_large(maximum.unwrap_or(i64::MAX)),
                Integer::Value(integer) => integer,
            };
            if let Some(min) = *minimum
                && integer < min
            {
                return too_small(min);
            }
            if let Some(max) = *maximum
                && integer > max
            {
                return too_large(max);
            }
            Check::Ok(Value::Number(Number::from(integer)))
        }
        FieldType::Boolean => match value {
            Value::Bool(_) => Check::Ok(value.clone()),
            _ => Check::Shape(type_error(at, "boolean")),
        },
        FieldType::Uuid => match value {
            Value::String(text) if is_uuid(text) => Check::Ok(value.clone()),
            Value::String(_) => Check::Invalid(
                FieldError::new(at, "format", "Enter a valid ID.")
                    .with_params(json!({ "format": "uuid" })),
            ),
            _ => Check::Shape(type_error(at, "string")),
        },
        FieldType::Digits => match value {
            Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                Check::Ok(value.clone())
            }
            Value::String(_) => Check::Invalid(
                FieldError::new(at, "format", "Enter digits only.")
                    .with_params(json!({ "format": "digits" })),
            ),
            _ => Check::Shape(type_error(at, "string")),
        },
        FieldType::Items { .. } => Check::Shape(type_error(at, "array")),
        FieldType::Decimal { precision, scale } => match value {
            Value::String(text) => match scalar::check_decimal(text, *precision, *scale) {
                Ok(()) => Check::Ok(value.clone()),
                Err(scalar::DecimalError::Syntax) => Check::Invalid(
                    FieldError::new(at, "format", "Enter a number, like 12.50.")
                        .with_params(json!({ "format": "decimal" })),
                ),
                Err(scalar::DecimalError::Digits) => {
                    let whole = precision - scale;
                    Check::Invalid(
                        FieldError::new(
                            at,
                            "decimal_digits",
                            format!(
                                "Use at most {whole} digits before the decimal point and {scale} after."
                            ),
                        )
                        .with_params(json!({ "precision": precision, "scale": scale })),
                    )
                }
            },
            _ => Check::Shape(type_error(at, "string")),
        },
        FieldType::Date => match value {
            Value::String(text) if scalar::parse_date(text).is_some() => Check::Ok(value.clone()),
            Value::String(_) => Check::Invalid(
                FieldError::new(at, "format", "Enter a date as YYYY-MM-DD.")
                    .with_params(json!({ "format": "date" })),
            ),
            _ => Check::Shape(type_error(at, "string")),
        },
        FieldType::Instant => match value {
            Value::String(text) => match scalar::normalize_instant(text) {
                Some(utc) => Check::Ok(Value::String(utc)),
                None => Check::Invalid(
                    FieldError::new(
                        at,
                        "format",
                        "Enter a date and time with a time zone, like 2026-10-17T09:30:00Z.",
                    )
                    .with_params(json!({ "format": "date-time" })),
                ),
            },
            _ => Check::Shape(type_error(at, "string")),
        },
        FieldType::Duration => match value {
            Value::String(text) if scalar::parse_duration(text).is_some() => {
                Check::Ok(value.clone())
            }
            Value::String(_) => Check::Invalid(
                FieldError::new(at, "format", "Enter a duration like PT30M or P1DT2H.")
                    .with_params(json!({ "format": "duration" })),
            ),
            _ => Check::Shape(type_error(at, "string")),
        },
        FieldType::Enum { values } => match value {
            Value::String(text) if values.contains(text) => Check::Ok(value.clone()),
            Value::String(_) => Check::Invalid(
                FieldError::new(
                    at,
                    "choice",
                    format!("Choose one of: {}.", values.join(", ")),
                )
                .with_params(json!({ "choices": values })),
            ),
            _ => Check::Shape(type_error(at, "string")),
        },
    }
}

/// Validates a list field, prefixing nested pointers with `at/<index>`.
fn check_items(
    item: &ContractDoc,
    max_items: u32,
    at: &str,
    value: &Value,
    shape: &mut Vec<FieldError>,
    invalid: &mut Vec<FieldError>,
) -> Option<Value> {
    let Value::Array(elements) = value else {
        shape.push(type_error(at, "array"));
        return None;
    };
    if elements.len() > max_items as usize {
        invalid.push(
            FieldError::new(at, "max_items", format!("Use {max_items} items or fewer."))
                .with_params(json!({ "max": max_items })),
        );
        return None;
    }
    let mut normalized = Vec::new();
    let mut ok = true;
    for (index, element) in elements.iter().enumerate() {
        let prefix = format!("{at}/{index}");
        match item.validate(element) {
            Ok(value) => normalized.push(value),
            Err(Rejection::NotObject) => {
                shape.push(type_error(&prefix, "object"));
                ok = false;
            }
            Err(Rejection::Shape(errors)) | Err(Rejection::Invalid(errors))
                if errors.is_empty() =>
            {
                ok = false
            }
            Err(Rejection::Shape(errors)) => {
                shape.extend(errors.into_iter().map(|mut e| {
                    e.pointer = format!("{prefix}{}", e.pointer);
                    e
                }));
                ok = false;
            }
            Err(Rejection::Invalid(errors)) => {
                invalid.extend(errors.into_iter().map(|mut e| {
                    e.pointer = format!("{prefix}{}", e.pointer);
                    e
                }));
                ok = false;
            }
        }
    }
    ok.then_some(Value::Array(normalized))
}

fn type_error(at: &str, expected: &str) -> FieldError {
    let article = if expected.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    FieldError::new(at, "type", format!("Expected {article} {expected}."))
        .with_params(json!({ "expected": expected }))
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

enum Integer {
    Value(i64),
    BelowI64,
    AboveI64,
    Fractional,
}

/// JSON Schema `integer` semantics: any number with no fractional part.
fn classify(number: &Number) -> Integer {
    if let Some(value) = number.as_i64() {
        return Integer::Value(value);
    }
    if number.as_u64().is_some() {
        return Integer::AboveI64;
    }
    match number.as_f64() {
        Some(f) if f.is_finite() && f.fract() == 0.0 => {
            if f >= 9_223_372_036_854_775_808.0 {
                Integer::AboveI64
            } else if f < -9_223_372_036_854_775_808.0 {
                Integer::BelowI64
            } else {
                Integer::Value(f as i64)
            }
        }
        _ => Integer::Fractional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lesson_input() -> ContractDoc {
        ContractDoc::input("lessons.create.input", "1.0.0")
            .field(Field::text("title").chars(1, 5))
            .field(Field::integer("grade").range(1, 12))
    }

    fn codes(rejection: &Rejection) -> Vec<(String, String)> {
        rejection
            .errors()
            .iter()
            .map(|e| (e.pointer.clone(), e.code.clone()))
            .collect()
    }

    fn pair(pointer: &str, code: &str) -> (String, String) {
        (pointer.to_owned(), code.to_owned())
    }

    #[test]
    fn accepts_and_normalizes() {
        let c = lesson_input();
        assert_eq!(
            c.validate(&json!({"title": "abc", "grade": 3.0})).unwrap(),
            json!({"title": "abc", "grade": 3})
        );
    }

    #[test]
    fn counts_code_points_not_bytes() {
        let c = lesson_input();
        assert!(
            c.validate(&json!({"title": "😀😀😀😀😀", "grade": 1}))
                .is_ok()
        );
        let err = c
            .validate(&json!({"title": "😀😀😀😀😀😀", "grade": 1}))
            .unwrap_err();
        assert_eq!(codes(&err), [pair("/title", "max_length")]);
    }

    #[test]
    fn missing_null_range_and_unknown_are_invalid_in_declaration_order() {
        let c = lesson_input();
        let err = c
            .validate(&json!({"grade": null, "school_id": "x", "a/b": 1}))
            .unwrap_err();
        assert!(matches!(err, Rejection::Invalid(_)));
        assert_eq!(
            codes(&err),
            [
                pair("/title", "required"),
                pair("/grade", "not_nullable"),
                pair("/a~1b", "unknown_field"),
                pair("/school_id", "unknown_field"),
            ]
        );
        let err = c.validate(&json!({"title": "", "grade": 13})).unwrap_err();
        assert_eq!(
            codes(&err),
            [pair("/title", "min_length"), pair("/grade", "maximum")]
        );
        for huge in [json!(u64::MAX), json!(1e20)] {
            let err = c
                .validate(&json!({"title": "a", "grade": huge}))
                .unwrap_err();
            assert_eq!(codes(&err), [pair("/grade", "maximum")]);
        }
    }

    #[test]
    fn wrong_types_are_shape_errors_and_take_precedence() {
        let c = lesson_input();
        let err = c
            .validate(&json!({"title": 5, "grade": "3", "extra": true}))
            .unwrap_err();
        assert!(matches!(err, Rejection::Shape(_)));
        assert_eq!(
            codes(&err),
            [pair("/title", "type"), pair("/grade", "type")]
        );
        assert!(matches!(
            c.validate(&json!({"title": "a", "grade": 2.5})),
            Err(Rejection::Shape(_))
        ));
        assert_eq!(c.validate(&json!([1, 2])), Err(Rejection::NotObject));
    }

    #[test]
    fn optional_nullable_and_formats() {
        let c = ContractDoc::output("lessons.view", "1.0.0")
            .field(Field::uuid("id"))
            .field(Field::digits("version"))
            .field(Field::enumeration("status", &["draft", "published"]))
            .field(Field::text("subtitle").optional().nullable())
            .field(Field::boolean("archived").optional());
        let ok = json!({
            "id": "01a0dc49-2bf1-7112-b58a-ea138be5bdf1",
            "version": "12",
            "status": "draft",
            "subtitle": null,
        });
        assert_eq!(c.validate(&ok).unwrap(), ok);
        let err = c
            .validate(&json!({"id": "nope", "version": "1.5", "status": "gone"}))
            .unwrap_err();
        assert_eq!(
            codes(&err),
            [
                pair("/id", "format"),
                pair("/version", "format"),
                pair("/status", "choice")
            ]
        );
    }

    #[test]
    fn json_schema_matches_specimen_shape() {
        let schema = lesson_input().json_schema();
        assert_eq!(
            schema,
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "additionalProperties": false,
                "required": ["grade", "title"],
                "properties": {
                    "title": {"type": "string", "minLength": 1, "maxLength": 5},
                    "grade": {"type": "integer", "minimum": 1, "maximum": 12},
                },
            })
        );
        let nullable = ContractDoc::output("a.b", "1")
            .field(Field::enumeration("s", &["x"]).nullable())
            .schema_body();
        assert_eq!(
            nullable["properties"]["s"],
            json!({"type": ["string", "null"], "enum": ["x", null]})
        );
    }

    #[test]
    fn digest_depends_on_rules_not_declaration_order() {
        let a = lesson_input();
        let mut b = lesson_input();
        b.fields.reverse();
        assert_eq!(a.digest(), b.digest());
        let c = ContractDoc::input("lessons.create.input", "1.0.0")
            .field(Field::text("title").chars(1, 6))
            .field(Field::integer("grade").range(1, 12));
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn example_satisfies_contract() {
        let c = ContractDoc::output("a.b", "1")
            .field(Field::text("t").chars(3, 9))
            .field(Field::integer("n").range(5, 9))
            .field(Field::uuid("u"))
            .field(Field::digits("d"))
            .field(Field::enumeration("e", &["x", "y"]))
            .field(Field::boolean("b").optional());
        assert!(c.validate(&c.example()).is_ok());
    }

    #[test]
    fn ir_round_trips() {
        let c = ContractDoc::output("a.b", "2")
            .field(Field::text("t").chars(3, 9).describe("Title"))
            .field(Field::integer("n").range(5, 9).optional())
            .field(Field::enumeration("e", &["x", "y"]).nullable())
            .field(Field::uuid("u"))
            .field(Field::digits("d"))
            .field(Field::boolean("b"));
        let back = ContractDoc::from_ir(&c.to_ir()).unwrap();
        assert_eq!(back.to_ir(), c.to_ir());
        assert_eq!(back.digest(), c.digest());
        assert!(ContractDoc::from_ir(&json!({"profile": "other"})).is_err());
    }

    #[test]
    fn compatibility_allows_widening_and_new_optional_fields_only() {
        let old = lesson_input();
        let widened = ContractDoc::input("lessons.create.input", "1.1.0")
            .field(Field::text("title").chars(1, 10))
            .field(Field::integer("grade").range(0, 12))
            .field(Field::text("subtitle").optional());
        assert!(widened.rejects_instances_of(&old).is_empty());

        let breaking = ContractDoc::input("lessons.create.input", "2.0.0")
            .field(Field::text("title").chars(2, 5))
            .field(Field::text("summary"));
        let reasons = breaking.rejects_instances_of(&old);
        assert_eq!(reasons.len(), 3, "{reasons:#?}");
        assert!(reasons[0].contains("title has a narrower length range"));
        assert!(reasons[1].contains("grade was removed"));
        assert!(reasons[2].contains("new field summary is required"));
    }

    #[test]
    fn lists_validate_elements_with_nested_pointers() {
        let item = ContractDoc::output("a.item", "1").field(Field::text("title").chars(1, 3));
        let page = ContractDoc::output("a.page", "1")
            .field(Field::items("items", &item, 2))
            .field(Field::text("next_cursor").nullable());
        let ok = json!({"items": [{"title": "ab"}], "next_cursor": null});
        assert_eq!(page.validate(&ok), Ok(ok.clone()));
        let err = page
            .validate(&json!({"items": [{"title": "abcd"}, {"title": ""}], "next_cursor": null}))
            .unwrap_err();
        assert_eq!(
            codes(&err),
            [
                pair("/items/0/title", "max_length"),
                pair("/items/1/title", "min_length")
            ]
        );
        let err = page
            .validate(&json!({"items": [{}, {}, {}], "next_cursor": null}))
            .unwrap_err();
        assert_eq!(codes(&err), [pair("/items", "max_items")]);
        assert_eq!(
            page.json_schema()["properties"]["items"]["items"]["properties"]["title"]["maxLength"],
            3
        );
        assert_eq!(
            ContractDoc::from_ir(&page.to_ir()).unwrap().digest(),
            page.digest()
        );
        assert!(page.validate(&page.example()).is_ok());
        let input = ContractDoc::input("a.in", "1").field(Field::items("items", &item, 2));
        assert!(
            input
                .problems()
                .iter()
                .any(|p| p.contains("only supported in output contracts"))
        );
    }

    #[test]
    fn definition_problems_are_reported() {
        let c = ContractDoc::input("bad", "")
            .field(Field::text("Title"))
            .field(Field::integer("n").range(5, 1))
            .field(Field::integer("n").chars(1, 2))
            .field(Field::enumeration("e", &[]));
        let problems = c.problems();
        assert_eq!(problems.len(), 7, "{problems:#?}");
        assert!(lesson_input().problems().is_empty());
    }
}
