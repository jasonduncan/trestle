//! Wire contracts (design doc §5.1). Each Rust type is bound to one
//! `ContractDoc`; the server validates with it and every export is generated
//! from it. Serde only maps the already-validated instance onto the type.
//! Optional inputs skip serialization when absent, so typed callers send
//! exactly what an HTTP client would.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use trestle_core::patch::Patch;
use trestle_core::protocol::{Contract, ContractDoc, Field};
use uuid::Uuid;

macro_rules! contract {
    ($ty:ty, $doc:expr) => {
        impl Contract for $ty {
            fn contract() -> &'static ContractDoc {
                static CONTRACT: LazyLock<ContractDoc> = LazyLock::new(|| $doc);
                &CONTRACT
            }
        }
    };
}

/// `lessons.create.input` 1.3.0 (1.1.0 added `subtitle`, 1.2.0 `unit_id`,
/// 1.3.0 `duration_minutes`; each optional, so earlier inputs still mean
/// the same thing).
///
/// Ownership is not part of the contract: the school comes from the tenant
/// context and the creator from the authenticated actor. Any other field,
/// including `school_id`, is rejected as unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateLessonInput {
    pub title: String,
    pub grade_level: i16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<i16>,
}

contract!(
    CreateLessonInput,
    ContractDoc::input("lessons.create.input", "1.3.0")
        .field(Field::text("title").chars(1, 200))
        .field(Field::integer("grade_level").range(1, 12))
        .field(Field::text("subtitle").chars(1, 200).optional())
        .field(Field::uuid("unit_id").optional())
        .field(Field::integer("duration_minutes").range(1, 600).optional())
);

/// `lessons.get.input` 1.0.0, bound to the `{id}` path parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetLessonInput {
    pub id: Uuid,
}

contract!(
    GetLessonInput,
    ContractDoc::input("lessons.get.input", "1.0.0").field(Field::uuid("id"))
);

/// `lessons.list.input` 1.0.0: allowlisted filters, a page size and a
/// cursor. Filters exist only for fields every reader may see (§12.3).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListLessonsInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grade_level: Option<i16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<LessonStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<Uuid>,
}

contract!(
    ListLessonsInput,
    ContractDoc::input("lessons.list.input", "1.0.0")
        .field(Field::text("cursor").chars(1, 1024).optional())
        .field(Field::integer("limit").range(1, 100).optional())
        .field(Field::integer("grade_level").range(1, 12).optional())
        .field(Field::enumeration("status", &["draft", "published"]).optional())
        .field(Field::uuid("unit_id").optional())
);

/// `lessons.update.input` 1.1.0 (§5.3; 1.1.0 added `duration_minutes`). `id` comes from the path and
/// `expected_version` from `If-Match`. Absent fields stay unchanged; `null`
/// clears nullable ones. Status, version and ownership are not fields here:
/// they change only through operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchLessonInput {
    pub id: Uuid,
    pub expected_version: String,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    pub title: Patch<String>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    pub grade_level: Patch<i16>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    pub subtitle: Patch<String>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    pub unit_id: Patch<Uuid>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    pub duration_minutes: Patch<i16>,
}

contract!(
    PatchLessonInput,
    ContractDoc::input("lessons.update.input", "1.1.0")
        .field(Field::uuid("id"))
        .field(Field::digits("expected_version"))
        .field(Field::text("title").chars(1, 200).optional())
        .field(Field::integer("grade_level").range(1, 12).optional())
        .field(Field::text("subtitle").chars(1, 200).optional().nullable())
        .field(Field::uuid("unit_id").optional().nullable())
        .field(
            Field::integer("duration_minutes")
                .range(1, 600)
                .optional()
                .nullable()
        )
);

/// `lessons.delete.input` 1.0.0: `id` from the path, version from `If-Match`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteLessonInput {
    pub id: Uuid,
    pub expected_version: String,
}

contract!(
    DeleteLessonInput,
    ContractDoc::input("lessons.delete.input", "1.0.0")
        .field(Field::uuid("id"))
        .field(Field::digits("expected_version"))
);

/// `lessons.publish.input` 1.0.0: a domain action, stale versions are 409 (§6.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishLessonInput {
    pub id: Uuid,
    pub expected_version: String,
}

contract!(
    PublishLessonInput,
    ContractDoc::input("lessons.publish.input", "1.0.0")
        .field(Field::uuid("id"))
        .field(Field::digits("expected_version"))
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LessonStatus {
    Draft,
    Published,
}

impl LessonStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "draft" => Some(LessonStatus::Draft),
            "published" => Some(LessonStatus::Published),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LessonStatus::Draft => "draft",
            LessonStatus::Published => "published",
        }
    }
}

/// `lessons.view` 1.3.0 (1.1.0 added `subtitle`, 1.2.0 `unit_id`, 1.3.0
/// `duration_minutes`; each null when absent). The version token is a string so JavaScript clients never
/// round it through a float (§5.4).
#[derive(Debug, Clone, Serialize)]
pub struct LessonView {
    pub id: Uuid,
    pub title: String,
    pub grade_level: i16,
    pub status: LessonStatus,
    pub version: String,
    pub subtitle: Option<String>,
    pub unit_id: Option<Uuid>,
    pub duration_minutes: Option<i16>,
}

contract!(
    LessonView,
    ContractDoc::output("lessons.view", "1.3.0")
        .field(Field::uuid("id"))
        .field(Field::text("title").chars(1, 200))
        .field(Field::integer("grade_level").range(1, 12))
        .field(Field::enumeration("status", &["draft", "published"]))
        .field(Field::digits("version"))
        .field(Field::text("subtitle").chars(1, 200).nullable())
        .field(Field::uuid("unit_id").nullable())
        .field(Field::integer("duration_minutes").range(1, 600).nullable())
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use trestle_core::input::decode;

    #[test]
    fn decodes_valid_input() {
        let input: CreateLessonInput =
            decode(&json!({"title": "Fractions", "grade_level": 4})).unwrap();
        assert_eq!(
            input,
            CreateLessonInput {
                title: "Fractions".into(),
                grade_level: 4,
                subtitle: None,
                unit_id: None,
                duration_minutes: None,
            }
        );
    }

    #[test]
    fn rejects_ownership_fields() {
        let err = decode::<CreateLessonInput>(&json!({
            "title": "Fractions",
            "grade_level": 4,
            "school_id": "22222222-2222-4222-8222-222222222222",
            "created_by": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        }))
        .unwrap_err();
        let pointers: Vec<_> = err.errors().iter().map(|e| e.pointer.as_str()).collect();
        assert_eq!(pointers, ["/created_by", "/school_id"]);
    }

    #[test]
    fn patch_keeps_missing_null_and_value_apart() {
        let id = "01a0dc49-2bf1-7112-b58a-ea138be5bdf1";
        let patch: PatchLessonInput =
            decode(&json!({"id": id, "expected_version": "3", "subtitle": null, "grade_level": 5}))
                .unwrap();
        assert!(patch.title.is_missing());
        assert_eq!(patch.subtitle, Patch::Null);
        assert_eq!(patch.grade_level, Patch::Value(5));
        let err =
            decode::<PatchLessonInput>(&json!({"id": id, "expected_version": "3", "title": null}))
                .unwrap_err();
        assert_eq!(err.errors()[0].code, "not_nullable");
        let err = decode::<PatchLessonInput>(
            &json!({"id": id, "expected_version": "3", "status": "published"}),
        )
        .unwrap_err();
        assert_eq!(err.errors()[0].code, "unknown_field");
    }

    #[test]
    fn view_serializes_within_its_contract() {
        let view = LessonView {
            id: Uuid::now_v7(),
            title: "Fractions".into(),
            grade_level: 4,
            status: LessonStatus::Draft,
            version: "1".into(),
            subtitle: None,
            unit_id: None,
            duration_minutes: Some(45),
        };
        let value = serde_json::to_value(&view).unwrap();
        assert_eq!(LessonView::contract().validate(&value), Ok(value));
    }
}
