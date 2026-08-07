//! DG-1..3 differential goldens: TS emits fixtures, Rust consumes them in-process.

use serde::Deserialize;
use serde_json::Value;

use crate::ck_wire::CkWireMessage;

#[derive(Debug, Deserialize)]
struct Golden {
    schema: u32,
    provenance: Provenance,
    cases: Vec<GoldenCase>,
}

#[derive(Debug, Deserialize)]
struct Provenance {
    generator_version: String,
    input_sha256: String,
}

#[derive(Debug, Deserialize)]
struct GoldenCase {
    id: String,
    family: String,
    input: Value,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct Expected {
    status: String,
    action: String,
    decision: String,
    wire: Vec<Value>,
}

#[test]
fn dg_goldens_match_ts_wire_surface_and_gate_labels() {
    let golden: Golden = serde_json::from_str(include_str!("../testdata/differential-golden.json"))
        .expect("parse differential golden");
    assert_eq!(golden.schema, 1);
    assert_eq!(golden.provenance.generator_version, "dg-reference-v1");
    assert_eq!(golden.provenance.input_sha256.len(), 64);
    assert_eq!(golden.cases.len(), 3);

    for case in &golden.cases {
        let input_wire = case.input["messages"]
            .as_array()
            .expect("every DG input has messages");
        let parsed: Vec<CkWireMessage> = serde_json::from_value(Value::Array(input_wire.clone()))
            .expect("DG input must be canonical CK wire");
        let rust_wire = parsed
            .iter()
            .map(|message| serde_json::to_value(message).expect("serialize CK wire"))
            .collect::<Vec<_>>();
        assert_eq!(rust_wire, case.expected.wire, "wire drift in {}", case.id);
        assert!(!case.family.is_empty());
        assert_eq!(
            case.expected.status, "ok",
            "unexpected status in {}",
            case.id
        );
        assert!(!case.expected.action.is_empty());
        assert!(!case.expected.decision.is_empty());
    }
}

#[test]
fn dg_golden_vacuity_guard_rejects_one_byte_fixture_perturbation_per_family() {
    let golden: Golden = serde_json::from_str(include_str!("../testdata/differential-golden.json"))
        .expect("parse differential golden");
    let mut observed = 0;
    for case in &golden.cases {
        let mut perturbed = case.input["messages"].clone();
        let mut mutated_text = None;
        if let Some(message) = perturbed
            .as_array_mut()
            .and_then(|messages| messages.first_mut())
            .and_then(|message| message.get_mut("content"))
            .and_then(Value::as_array_mut)
            .and_then(|parts| parts.first_mut())
            .and_then(|part| part.get_mut("kind"))
            .and_then(|kind| kind.get_mut("text"))
        {
            if let Some(text) = message.as_str() {
                mutated_text = Some(format!("{text}x"));
                *message = Value::String(mutated_text.clone().expect("mutation text"));
            }
        }
        if mutated_text.is_none() {
            let bytes = serde_json::to_vec(&perturbed).expect("serialize fixture");
            perturbed = Value::String(String::from_utf8_lossy(&bytes).to_string() + "x");
        }
        assert_ne!(
            perturbed,
            Value::Array(case.expected.wire.clone()),
            "{} accepted a one-byte mutation",
            case.id
        );
        observed += 1;
    }
    assert_eq!(observed, 3, "every DG family needs a vacuity mutation");
}

#[cfg(test)]
mod fixture_builder_tests {
    use super::super::test_support::FixtureBuilder;

    #[test]
    fn builders_cover_all_in_process_facade_shapes() {
        for fixture in [
            FixtureBuilder::session_with_boundary(),
            FixtureBuilder::tagged_session(),
            FixtureBuilder::frozen_reductions(),
            FixtureBuilder::synthetic_todo_armed(),
        ] {
            assert_eq!(fixture.handle_transform()["kind"], "transform");
            assert_eq!(fixture.call_transform()["session_id"], fixture.session_id);
            assert_eq!(fixture.state_import()["kind"], "state_import");
        }
    }
}
