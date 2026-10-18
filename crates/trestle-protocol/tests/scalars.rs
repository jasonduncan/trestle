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

/// ADR-028: tagged unions, from `fixtures/contracts/unions.json`.
#[test]
fn unions_agree_with_every_fixture() {
    agree(include_str!("../../../fixtures/contracts/unions.json"));
}

#[test]
fn unions_round_trip_check_their_variants_and_evolve_by_variant() {
    use trestle_protocol::Field;
    let fixture: Value =
        serde_json::from_str(include_str!("../../../fixtures/contracts/unions.json")).unwrap();
    let doc = ContractDoc::from_ir(&fixture["ir"]).unwrap();
    assert_eq!(doc.to_ir(), fixture["ir"]);
    assert!(doc.validate(&doc.example()).is_ok(), "the example is valid");

    let card = ContractDoc::input("x.card", "1").field(Field::text("last4").chars(4, 4));
    let cash = ContractDoc::input("x.cash", "1");
    let pay = |variants: &[(&str, &ContractDoc)]| {
        ContractDoc::input("x.pay", "1").field(Field::union("payment", "method", variants))
    };
    let base = pay(&[("card", &card), ("cash", &cash)]);
    assert!(base.problems().is_empty(), "{:?}", base.problems());
    assert_eq!(
        pay(&[("cash", &cash), ("card", &card)]),
        base,
        "variants are kept in name order"
    );
    let wider = pay(&[("card", &card), ("cash", &cash), ("gift", &cash)]);
    assert!(
        wider.rejects_instances_of(&base).is_empty(),
        "a new variant is compatible"
    );
    assert_eq!(
        pay(&[("card", &card)]).rejects_instances_of(&base),
        ["field payment dropped variant cash"]
    );
    let strict = ContractDoc::input("x.card", "2")
        .field(Field::text("last4").chars(4, 4))
        .field(Field::text("zip"));
    assert_eq!(
        pay(&[("card", &strict), ("cash", &cash)]).rejects_instances_of(&base),
        ["field payment, variant card: new field zip is required"]
    );
    let retagged = ContractDoc::input("x.pay", "1").field(Field::union(
        "payment",
        "kind",
        &[("card", &card), ("cash", &cash)],
    ));
    assert_eq!(
        retagged.rejects_instances_of(&base),
        ["field payment changed its tag from method to kind"]
    );

    let tagged = ContractDoc::input("x.card", "1").field(Field::text("method"));
    let nested =
        ContractDoc::input("x.nested", "1").field(Field::union("inner", "t", &[("a", &cash)]));
    let problems = |doc: ContractDoc| doc.problems().join("\n");
    assert!(problems(pay(&[])).contains("at least one variant"));
    assert!(problems(pay(&[("card", &tagged)])).contains("variant card declares the tag method"));
    assert!(problems(pay(&[("deep", &nested)])).contains("variant deep holds a list or a union"));
    assert!(problems(pay(&[("Card", &cash)])).contains("unique identifiers"));
    assert!(problems(pay(&[("card", &cash), ("card", &card)])).contains("unique identifiers"));
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
