//! F6: the auto-apply classifier (design §7.1), one row per operation.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use trestle_historical::classify::{Reason, classify, generated_sql, op_reasons};
use trestle_historical::schema::{SchemaState, StateOp};
use trestle_historical::{DataStep, Migration};

fn op(v: Value) -> StateOp {
    serde_json::from_value(v).unwrap()
}

fn field(extra: Value) -> Value {
    let mut f =
        json!({"id": "shop.item.extra", "column": "extra", "type": "text", "nullable": true});
    for (k, v) in extra.as_object().unwrap() {
        f[k] = v.clone();
    }
    f
}

fn create_item() -> StateOp {
    op(json!({"op": "create_model", "model": {
        "id": "shop.item", "table": "items",
        "fields": [{"id": "shop.item.id", "column": "id", "type": "uuid", "nullable": false, "primary_key": true}]
    }}))
}

fn add_field(extra: Value) -> StateOp {
    op(json!({"op": "add_field", "model": "shop.item", "field": field(extra)}))
}

#[test]
fn every_operation_is_classified() {
    use Reason::*;
    let index = op(
        json!({"op": "add_index", "model": "shop.item", "index": {"name": "items_x_idx", "fields": ["shop.item.id"]}}),
    );
    let unique = op(
        json!({"op": "add_unique", "model": "shop.item", "unique": {"name": "items_x_key", "fields": ["shop.item.id"]}}),
    );
    let rows: Vec<(&str, StateOp, Vec<Reason>)> = vec![
        ("create_model", create_item(), vec![]),
        ("add_field nullable", add_field(json!({})), vec![]),
        (
            "add_field NOT NULL constant default",
            add_field(json!({"nullable": false, "default": {"kind": "bool", "value": false}})),
            vec![],
        ),
        (
            "add_field NOT NULL no default",
            add_field(json!({"nullable": false})),
            vec![RewriteRisk],
        ),
        (
            "add_field volatile default",
            add_field(json!({"default": {"kind": "now"}})),
            vec![RewriteRisk],
        ),
        (
            "add_field foreign key",
            add_field(json!({"type": "uuid", "references": "shop.other"})),
            vec![ExistingTableLock],
        ),
        (
            "add_index existing table",
            index.clone(),
            vec![ExistingTableLock],
        ),
        ("add_unique existing table", unique, vec![ExistingTableLock]),
        (
            "alter_check",
            op(
                json!({"op": "alter_check", "model": "shop.item", "field": "shop.item.extra", "check": null, "check_name": null}),
            ),
            vec![ExistingTableLock],
        ),
        (
            "alter_nullability",
            op(
                json!({"op": "alter_nullability", "model": "shop.item", "field": "shop.item.extra", "nullable": false}),
            ),
            vec![RewriteRisk],
        ),
        (
            "backfill",
            op(
                json!({"op": "backfill", "model": "shop.item", "field": "shop.item.extra", "value": "x"}),
            ),
            vec![DataStep],
        ),
        (
            "remove_model",
            op(json!({"op": "remove_model", "model": "shop.item"})),
            vec![Destructive],
        ),
        (
            "remove_field",
            op(json!({"op": "remove_field", "model": "shop.item", "field": "shop.item.extra"})),
            vec![Destructive],
        ),
        (
            "remove_index",
            op(json!({"op": "remove_index", "model": "shop.item", "name": "items_x_idx"})),
            vec![Destructive],
        ),
        (
            "remove_unique",
            op(json!({"op": "remove_unique", "model": "shop.item", "name": "items_x_key"})),
            vec![Destructive],
        ),
        (
            "rename_table",
            op(json!({"op": "rename_table", "model": "shop.item", "table": "things"})),
            vec![Destructive],
        ),
        (
            "rename_column",
            op(
                json!({"op": "rename_column", "model": "shop.item", "field": "shop.item.extra", "column": "more"}),
            ),
            vec![Destructive],
        ),
    ];
    let none = BTreeSet::new();
    for (name, op, expected) in rows {
        assert_eq!(op_reasons(&op, &none), expected, "{name}");
    }
    // On a table created in the same migration, indexes and FKs are free.
    let created: BTreeSet<String> = ["shop.item".to_owned()].into();
    assert_eq!(op_reasons(&index, &created), vec![]);
    assert_eq!(
        op_reasons(
            &add_field(json!({"type": "uuid", "references": "shop.other"})),
            &created
        ),
        vec![]
    );
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn migration(ops: &[StateOp], sql: Option<&str>, extra: &str) -> Migration {
    let rendered = generated_sql("shop", "0001_initial", &SchemaState::default(), ops).unwrap();
    let manifest = json!({"format": 2, "app": "shop", "id": "0001_initial", "operations": ops});
    let mut manifest = manifest.to_string();
    manifest.pop();
    manifest.push_str(extra);
    manifest.push('}');
    Migration {
        app: "shop",
        id: "0001_initial",
        depends_on: &[],
        sql: leak(sql.map(str::to_owned).unwrap_or(rendered)),
        manifest: Some(leak(manifest)),
        data: None,
    }
}

#[test]
fn generated_sql_is_auto_and_edits_need_approval() {
    let ops = [create_item()];
    let empty = SchemaState::default();
    let classify_one = |m: &Migration| classify(m, Some(&ops), Some(&empty));

    assert!(classify_one(&migration(&ops, None, "")).is_empty());
    assert_eq!(
        classify_one(&migration(&ops, Some("CREATE TABLE items (id uuid);"), "")),
        [Reason::HandSql]
    );
    assert_eq!(
        classify_one(&migration(&ops, None, r#", "atomic": false"#)),
        [Reason::NonAtomic]
    );
    let mut with_data = migration(&ops, None, "");
    with_data.data = Some(DataStep {
        name: "fill",
        source: "",
        run: |_| Box::pin(async { Ok(()) }),
    });
    assert_eq!(classify_one(&with_data), [Reason::DataStep]);
    // No manifest: nothing to check the SQL against.
    let mut bare = migration(&ops, None, "");
    bare.manifest = None;
    assert_eq!(classify(&bare, None, None), [Reason::HandSql]);
}
