//! Spec mem_1ChUb85e8Zxt8vYkyUonFe: form schema conversion and PromptCard answer round trips.
use agent_chat_acp::elicitation::{build_elicitation_content, parse_elicitation_questions};
use serde_json::json;

#[test]
fn question_keys_are_unique_even_when_generated_suffixes_collide() {
    let (questions, fields) = parse_elicitation_questions(&json!({"message":"Pick", "requestedSchema":{"properties":{
        "a":{"type":"string"},"b":{"type":"string"},"c":{"type":"string","description":"Pick (b: 1)"}
    }}})).unwrap();
    let mut answers = serde_json::Map::new();
    for (i, q) in questions.iter().enumerate() {
        assert!(
            answers
                .insert(q.question.clone(), json!(i.to_string()))
                .is_none()
        );
    }
    assert_eq!(
        build_elicitation_content(&fields, &answers.into()),
        json!({"a":"0","b":"1","c":"2"})
    );
}

#[test]
fn all_form_field_types_round_trip() {
    let (questions, fields) = parse_elicitation_questions(&json!({"mode":"form","message":"Values", "requestedSchema":{"type":"object","properties":{
        "enum":{"type":"string","title":"Enum","enum":["x","y"],"enumNames":["First","Second"]},
        "one":{"type":"string","title":"One","oneOf":[{"const":"internal","title":"Friendly"}]},
        "many":{"type":"array","title":"Many","items":{"anyOf":[{"const":"a","title":"A"},{"const":"b","title":"B"}]}},
        "bool":{"type":"boolean","title":"Bool"},
        "integer":{"type":"integer","title":"Integer","default":3},
        "number":{"type":"number","title":"Number"},
        "text":{"type":"string","title":"Text"}
    }}})).unwrap();
    assert_eq!(
        questions
            .iter()
            .find(|q| q.question == "Enum")
            .unwrap()
            .options[0]
            .description,
        "First"
    );
    assert_eq!(
        questions
            .iter()
            .find(|q| q.question == "One")
            .unwrap()
            .options[0]
            .description,
        "Friendly"
    );
    assert!(
        questions
            .iter()
            .find(|q| q.question == "Many")
            .unwrap()
            .multi_select
    );
    assert!(
        questions
            .iter()
            .find(|q| q.question == "Text")
            .unwrap()
            .options
            .is_empty()
    );
    assert_eq!(
        build_elicitation_content(
            &fields,
            &json!({"Enum":"y","One":"internal","Many":"a, b","Bool":"Yes","Integer":"42","Number":"1.25","Text":"hello"})
        ),
        json!({"enum":"y","one":"internal","many":["a","b"],"bool":true,"integer":42,"number":1.25,"text":"hello"})
    );
}

#[test]
fn unsupported_forms_fail_explicitly() {
    for form in [
        json!({"mode":"url"}),
        json!({}),
        json!({"requestedSchema":{"properties":{}}}),
        json!({"requestedSchema":{"properties":{"x":{"type":"object"}}}}),
    ] {
        assert!(parse_elicitation_questions(&form).is_err());
    }
}

#[test]
fn array_options_with_commas_round_trip_together() {
    let (_, fields) = parse_elicitation_questions(&json!({"requestedSchema":{"properties":{"x":{"title":"X","type":"array","items":{"enum":["a, b","c"]}}}}})).unwrap();
    assert_eq!(
        build_elicitation_content(&fields, &json!({"X":"a, b, c"})),
        json!({"x":["a, b","c"]})
    );
}
