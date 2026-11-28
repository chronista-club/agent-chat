//! ACP の form elicitation ↔ PromptCard の写し。
//!
//! 質問文が UI の回答 map のキーになる。schema の property 名と型は外に出さず、保留中の request 側
//! （[`ElicitationFieldKind`]）に持って、回答を content へ戻す時に使う。
//!
//! agy は選択式の質問 `<key>` ごとに、自由記述の受け口 `<key>_other`（選択肢の無い string）を対で
//! 出す。PromptCard は選択肢の末尾に自前の「Other」入力を持つので、対の `<key>_other` は別の質問に
//! せず `<key>` に畳み、`<key>` の選択肢にある "Other" も外す（どちらも残すと Other が 2 つ並び、
//! 下段の質問に何か書かないと確定できなかった）。回答が選択肢に無ければ自由記述として `<key>_other`
//! に戻す。
use std::collections::{HashMap, HashSet};

use agent_chat_protocol::{QuestionOption, QuestionSpec};
use serde_json::{Map, Value};

/// form の 1 field（property 名・質問文・型・選択肢）。回答を content へ戻す時に使う。
#[derive(Debug, Clone)]
pub struct ElicitationFieldKind {
    key: String,
    question: String,
    kind: String,
    /// 選択肢の（PromptCard に出す label, content に戻す値）。`oneOf` は label = title・値 = const。
    options: Vec<(String, String)>,
    /// 対の自由記述 field（agy の `<key>_other`）。回答が選択肢に無い時はここへ入れる。
    write_in_key: Option<String>,
    /// `<key>` の選択肢から外した "Other" の値。自由記述の時に `<key>` 側へ入れる（無ければ入れない）。
    other_option: Option<String>,
}

/// 選択肢の無い string か（自由記述の受け口になれる形か）。
fn is_free_text(prop: &Value) -> bool {
    prop["type"].as_str().unwrap_or("string") == "string"
        && ["enum", "oneOf", "anyOf"]
            .iter()
            .all(|k| prop.get(*k).is_none())
}

/// `<key>_other` の対を探す（自由記述 key → 選択式の `<key>`）。
fn write_in_pairs(properties: &Map<String, Value>) -> HashMap<String, String> {
    properties
        .iter()
        .filter_map(|(key, prop)| {
            let base = key.strip_suffix("_other")?;
            let base_prop = properties.get(base)?;
            let base_kind = base_prop["type"].as_str().unwrap_or("string");
            let source = if base_kind == "array" {
                &base_prop["items"]
            } else {
                base_prop
            };
            let has_choices = ["enum", "oneOf", "anyOf"]
                .iter()
                .any(|k| source.get(*k).is_some());
            (matches!(base_kind, "string" | "array") && has_choices && is_free_text(prop))
                .then(|| (key.clone(), base.to_owned()))
        })
        .collect()
}

/// `elicitation/create` の params を PromptCard の質問へ写す（純関数）。form 以外の mode・
/// 未対応の型は Err（host が invalid_params で断る）。
pub fn parse_elicitation_questions(
    params: &Value,
) -> Result<(Vec<QuestionSpec>, Vec<ElicitationFieldKind>), String> {
    if params.get("mode").is_some_and(|mode| mode != "form") {
        return Err("Only form elicitation is supported".into());
    }
    let schema = &params["requestedSchema"];
    if schema.get("type").is_some_and(|t| t != "object") {
        return Err("Expected an object schema".into());
    }
    let properties = schema["properties"]
        .as_object()
        .ok_or("Missing form properties")?;
    if properties.is_empty() {
        return Err("Empty form".into());
    }
    let pairs = write_in_pairs(properties);
    let write_in_of: HashMap<&str, &str> = pairs
        .iter()
        .map(|(other, base)| (base.as_str(), other.as_str()))
        .collect();
    let mut used = HashSet::new();
    let mut questions = Vec::new();
    let mut fields = Vec::new();
    for (key, prop) in properties {
        if pairs.contains_key(key) {
            continue;
        }
        let write_in_key = write_in_of.get(key.as_str()).map(|k| (*k).to_owned());
        let kind = prop["type"].as_str().unwrap_or("string");
        if !matches!(kind, "string" | "number" | "integer" | "boolean" | "array") {
            return Err(format!("Unsupported field type: {kind}"));
        }
        let title = prop["title"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(key);
        // agy の対では title が質問文本体で、description は「Other を選んで下に書く」定型文。
        let description = prop["description"]
            .as_str()
            .filter(|s| !s.is_empty() && write_in_key.is_none());
        let base = description
            .or_else(|| prop["title"].as_str().filter(|s| !s.is_empty()))
            .or_else(|| params["message"].as_str().filter(|s| !s.is_empty()))
            .unwrap_or(key);
        let mut question = base.to_string();
        let mut suffix = 1;
        while !used.insert(question.clone()) {
            question = format!("{base} ({key}: {suffix})");
            suffix += 1;
        }
        let mut other_option = None;
        let mut options = if kind == "boolean" {
            vec![
                (
                    QuestionOption {
                        label: "Yes".into(),
                        description: "true".into(),
                    },
                    "Yes".to_owned(),
                ),
                (
                    QuestionOption {
                        label: "No".into(),
                        description: "false".into(),
                    },
                    "No".to_owned(),
                ),
            ]
        } else {
            let source = if kind == "array" {
                &prop["items"]
            } else {
                prop
            };
            if kind == "array" && source.get("type").is_some_and(|v| v != "string") {
                return Err("Only string arrays are supported".into());
            }
            let mut choices = Vec::new();
            if let Some(values) = source.get("enum").and_then(Value::as_array) {
                for (i, value) in values.iter().enumerate() {
                    let label = value.as_str().ok_or("Enum values must be strings")?;
                    choices.push((
                        QuestionOption {
                            label: label.into(),
                            description: source["enumNames"][i].as_str().unwrap_or("").into(),
                        },
                        label.to_owned(),
                    ));
                }
            } else if let Some(values) = source
                .get("oneOf")
                .or_else(|| source.get("anyOf"))
                .and_then(Value::as_array)
            {
                for value in values {
                    let constant = value["const"]
                        .as_str()
                        .ok_or("Options must have string const values")?;
                    // ボタンには title を出し、回答は const へ逆引きして返す。
                    let label = value["title"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(constant);
                    choices.push((
                        QuestionOption {
                            label: label.into(),
                            description: value["description"].as_str().unwrap_or("").into(),
                        },
                        constant.to_owned(),
                    ));
                }
                // title が重なると逆引きできないので、その時は const を label に戻す。
                let mut seen = HashSet::new();
                if !choices.iter().all(|(o, _)| seen.insert(o.label.clone())) {
                    for (option, constant) in &mut choices {
                        option.label = constant.clone();
                    }
                }
            } else if kind != "array"
                && let Some(default) = prop.get("default")
            {
                let label = default
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| default.to_string());
                choices.push((
                    QuestionOption {
                        label: label.clone(),
                        description: String::new(),
                    },
                    label,
                ));
            }
            choices
        };
        if write_in_key.is_some()
            && let Some(i) = options.iter().position(|(o, v)| {
                o.label.eq_ignore_ascii_case("other") || v.eq_ignore_ascii_case("other")
            })
        {
            other_option = Some(options.remove(i).1);
        }
        let paired = write_in_key.is_some();
        fields.push(ElicitationFieldKind {
            key: key.clone(),
            question: question.clone(),
            kind: kind.into(),
            options: options
                .iter()
                .map(|(o, v)| (o.label.clone(), v.clone()))
                .collect(),
            write_in_key,
            other_option,
        });
        questions.push(QuestionSpec {
            // 対の時は title を質問文に使ったので、見出しで繰り返さない。
            header: if title.is_empty() || paired {
                "Question".into()
            } else {
                title.into()
            },
            question,
            options: options.into_iter().map(|(o, _)| o).collect(),
            multi_select: kind == "array",
        });
    }
    Ok((questions, fields))
}

/// PromptCard の回答 map（質問文 → 回答）を、form の property 名をキーにした content へ戻す（純関数）。
///
/// 選択肢の label は値（`oneOf` の const）へ戻す。対の自由記述 field を持つ質問では、選択肢に無い
/// 回答（PromptCard の Other）を `<key>_other` へ入れ、`<key>` には外しておいた "Other" を入れる。
pub fn build_elicitation_content(fields: &[ElicitationFieldKind], answers: &Value) -> Value {
    let mut content = Map::new();
    for field in fields {
        let Some(answer) = answers.get(&field.question).and_then(Value::as_str) else {
            continue;
        };
        let value_of = |label: &str| {
            field
                .options
                .iter()
                .find(|(l, v)| l == label || v == label)
                .map(|(_, v)| v.clone())
        };
        let value = match field.kind.as_str() {
            "array" => {
                // enum の値 1 つの中に PromptCard の区切り文字が入っていることがある。
                let mut chosen = Vec::new();
                let mut written = Vec::new();
                let mut remaining = answer;
                while !remaining.is_empty() {
                    let matched = field
                        .options
                        .iter()
                        .map(|(label, _)| label.as_str())
                        .filter(|label| {
                            !label.is_empty()
                                && (remaining == *label
                                    || remaining
                                        .strip_prefix(*label)
                                        .is_some_and(|tail| tail.starts_with(", ")))
                        })
                        .max_by_key(|label| label.len());
                    let (value, tail) = if let Some(label) = matched {
                        (
                            label,
                            remaining[label.len()..].strip_prefix(", ").unwrap_or(""),
                        )
                    } else {
                        remaining.split_once(", ").unwrap_or((remaining, ""))
                    };
                    match value_of(value) {
                        Some(v) => chosen.push(v),
                        None if field.write_in_key.is_some() => written.push(value),
                        None => chosen.push(value.to_owned()),
                    }
                    remaining = tail;
                }
                if let Some(write_in_key) = &field.write_in_key
                    && !written.is_empty()
                {
                    // 自由記述の中の ", " で割れた分を戻す。
                    content.insert(write_in_key.clone(), Value::String(written.join(", ")));
                    chosen.extend(field.other_option.clone());
                }
                Value::Array(chosen.into_iter().map(Value::String).collect())
            }
            "boolean" => match answer {
                "Yes" | "true" => Value::Bool(true),
                "No" | "false" => Value::Bool(false),
                _ => Value::String(answer.into()),
            },
            "integer" => answer
                .parse::<i64>()
                .map(|v| Value::Number(v.into()))
                .unwrap_or_else(|_| Value::String(answer.into())),
            "number" => answer
                .parse::<serde_json::Number>()
                .map(Value::Number)
                .unwrap_or_else(|_| Value::String(answer.into())),
            _ => match (value_of(answer), &field.write_in_key) {
                (Some(v), _) => Value::String(v),
                (None, Some(write_in_key)) => {
                    content.insert(write_in_key.clone(), Value::String(answer.into()));
                    match &field.other_option {
                        Some(other) => Value::String(other.clone()),
                        None => continue,
                    }
                }
                (None, None) => Value::String(answer.into()),
            },
        };
        content.insert(field.key.clone(), value);
    }
    Value::Object(content)
}
