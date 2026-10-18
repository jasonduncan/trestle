//! ADR-022: the portable scalar types agree everywhere. Every case in
//! `fixtures/contracts/scalars.json` is checked against the server's
//! validation (status, codes, order and normalized value) and an
//! independent JSON Schema validator; `frontend/interaction-react` runs the
//! same cases through the browser validator.

use serde_json::{Value, json};
use trestle_protocol::{ContractDoc, Rejection};

const FIXTURE: &str = include_str!("../../../fixtures/contracts/scalars.json");

fn outcome(result: Result<Value, Rejection>) -> (Value, Option<Value>) {
    let errors = |status: u16, code: &str, errors: &[trestle_protocol::FieldError]| {
        let pairs: Vec<Value> = errors.iter().map(|e| json!([e.pointer, e.code])).collect();
        json!({ "status": status, "code": code, "errors": pairs })
    };
    match result {
        Ok(value) => (json!({ "status": 201 }), Some(value)),
        Err(Rejection::NotObject) => (json!({ "status": 400 }), None),
        Err(Rejection::Shape(e)) => (errors(400, "invalid_body_shape", &e), None),
        Err(Rejection::Invalid(e)) => (errors(422, "validation_failed", &e), None),
    }
}

#[test]
fn the_server_and_json_schema_agree_with_every_fixture() {
    agree(FIXTURE);
}

/// ADR-027: money and references, from `fixtures/contracts/composites.json`.
#[test]
fn composites_agree_with_every_fixture() {
    agree(include_str!("../../../fixtures/contracts/composites.json"));
}

fn agree(text: &str) {
    let fixture: Value = serde_json::from_str(text).unwrap();
    let doc = ContractDoc::from_ir(&fixture["ir"]).unwrap();
    assert!(doc.problems().is_empty(), "{:?}", doc.problems());
    assert_eq!(
        doc.json_schema(),
        fixture["schema"],
        "the fixture's schema is what the contract exports"
    );
    let schema = jsonschema::options()
        .should_validate_formats(true)
        .build(&fixture["schema"])
        .unwrap();

    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let expect = &case["expect"];
        let (got, normalized) = outcome(doc.validate(&case["input"]));
        let mut want = expect.clone();
        let want_normalized = want.as_object_mut().unwrap().remove("normalized");
        assert_eq!(got, want, "{name}");
        if let Some(n) = want_normalized {
            assert_eq!(normalized.as_ref(), Some(&n), "{name}: normalized value");
        }
        assert_eq!(
            schema.is_valid(&case["input"]),
            expect["status"] == 201,
            "{name}: the published schema disagrees"
        );
    }
}

#[test]
fn the_ir_round_trips_and_older_clients_are_judged_by_digits() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let doc = ContractDoc::from_ir(&fixture["ir"]).unwrap();
    assert_eq!(doc.to_ir(), fixture["ir"]);
    let wider = ContractDoc::input("fixtures.scalars.input", "2")
        .field(trestle_protocol::Field::decimal("price", 12, 3));
    let narrow = ContractDoc::input("fixtures.scalars.input", "2")
        .field(trestle_protocol::Field::decimal("price", 10, 1));
    let base = ContractDoc::input("fixtures.scalars.input", "1")
        .field(trestle_protocol::Field::decimal("price", 10, 2));
    assert!(wider.rejects_instances_of(&base).is_empty());
    assert_eq!(
        narrow.rejects_instances_of(&base),
        ["field price accepts fewer decimal digits"]
    );
    let bad = ContractDoc::input("fixtures.bad.input", "1")
        .field(trestle_protocol::Field::decimal("x", 0, 0))
        .field(trestle_protocol::Field::decimal("y", 2, 3));
    assert_eq!(bad.problems().len(), 2, "{:?}", bad.problems());
    assert!(doc.validate(&doc.example()).is_ok(), "the example is valid");
}
