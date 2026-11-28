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
            .label,
        "Friendly",
        "oneOf はボタンに title を出す"
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
    // PromptCard が返すのは title の label。const へ逆引きして返す。
    assert_eq!(
        build_elicitation_content(&fields, &json!({"One":"Friendly","Many":"A, B"})),
        json!({"one":"internal","many":["a","b"]})
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

/// 実機 agy acp 1.5.19 の形 A: `enum` に "Other" を含み、自由記述は `q0_other` に分かれる。
fn agy_enum_form(kind: &str) -> serde_json::Value {
    let q0 = if kind == "array" {
        json!({"type":"array","title":"Red or Blue?","description":"Select one or more options, or choose Other and fill in the text field below.","items":{"type":"string","enum":["Red","Blue","Other"]}})
    } else {
        json!({"type":"string","title":"Red or Blue?","description":"Select an option, or choose Other and fill in the text field below.","enum":["Red","Blue","Other"]})
    };
    json!({"message":"Red or Blue?","mode":"form","requestedSchema":{"type":"object","required":["q0"],"properties":{
        "q0": q0,
        "q0_other":{"type":"string","title":"Other (Red or Blue?)","description":"Custom answer for \"Red or Blue?\" when Other is selected."}
    }},"sessionId":"2d350762-9756-48ef-9672-415d03b8b256"})
}

/// 実機 agy acp の形 B: `oneOf` の const は番号、表示名は title。"Other" の const は無い。
fn agy_one_of_form() -> serde_json::Value {
    json!({"sessionId":"81e716ea-2e20-424c-a3e0-fd205094b400","toolCallId":"call_3341386147183502965","mode":"form","message":"The agent has a question.","requestedSchema":{"type":"object","properties":{
        "q1":{"type":"string","title":"Red or Blue?","oneOf":[{"const":"1","title":"Red"},{"const":"2","title":"Blue"}]},
        "q1_other":{"type":"string","title":"Other","description":"Optional free-form answer to: Red or Blue?"}
    }}})
}

#[test]
fn agy_other_pair_becomes_one_question_without_duplicate_other() {
    let (questions, _) = parse_elicitation_questions(&agy_enum_form("string")).unwrap();
    assert_eq!(
        questions.len(),
        1,
        "q0_other は独立した質問にしない: {questions:?}"
    );
    assert_eq!(questions[0].question, "Red or Blue?");
    let labels: Vec<_> = questions[0]
        .options
        .iter()
        .map(|o| o.label.as_str())
        .collect();
    assert_eq!(
        labels,
        ["Red", "Blue"],
        "PromptCard 自身の Other と重複させない"
    );
}

#[test]
fn agy_enum_pair_answers_choice_or_write_in() {
    let (questions, fields) = parse_elicitation_questions(&agy_enum_form("string")).unwrap();
    let q = &questions[0].question;
    assert_eq!(
        build_elicitation_content(&fields, &json!({ q: "Blue" })),
        json!({"q0":"Blue"})
    );
    assert_eq!(
        build_elicitation_content(&fields, &json!({ q: "Purple, please" })),
        json!({"q0":"Other","q0_other":"Purple, please"})
    );
}

#[test]
fn agy_enum_pair_multi_select_splits_choices_and_write_in() {
    let (questions, fields) = parse_elicitation_questions(&agy_enum_form("array")).unwrap();
    assert_eq!(questions.len(), 1);
    assert!(questions[0].multi_select);
    let q = &questions[0].question;
    assert_eq!(
        build_elicitation_content(&fields, &json!({ q: "Red, Blue" })),
        json!({"q0":["Red","Blue"]})
    );
    assert_eq!(
        build_elicitation_content(&fields, &json!({ q: "Red, Green, ish" })),
        json!({"q0":["Red","Other"],"q0_other":"Green, ish"})
    );
}

#[test]
fn agy_one_of_pair_shows_titles_and_returns_consts() {
    let (questions, fields) = parse_elicitation_questions(&agy_one_of_form()).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].question, "Red or Blue?");
    let labels: Vec<_> = questions[0]
        .options
        .iter()
        .map(|o| o.label.as_str())
        .collect();
    assert_eq!(labels, ["Red", "Blue"]);
    let q = &questions[0].question;
    assert_eq!(
        build_elicitation_content(&fields, &json!({ q: "Blue" })),
        json!({"q1":"2"})
    );
    // "Other" の const が無いので、自由記述は q1_other だけに入る。
    assert_eq!(
        build_elicitation_content(&fields, &json!({ q: "Green" })),
        json!({"q1_other":"Green"})
    );
}

#[test]
fn other_suffix_without_base_stays_a_plain_question() {
    let (questions, fields) =
        parse_elicitation_questions(&json!({"requestedSchema":{"properties":{
            "note_other":{"type":"string","title":"Note"}
        }}}))
        .unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(
        build_elicitation_content(&fields, &json!({"Note":"hi"})),
        json!({"note_other":"hi"})
    );
}
