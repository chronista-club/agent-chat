//! Form elicitation ↔ PromptCard. Question text is the UI answer key; property names stay private.
use std::collections::HashSet;

use agent_chat_protocol::{QuestionOption, QuestionSpec};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct ElicitationFieldKind {
    key: String,
    question: String,
    kind: String,
    options: Vec<String>,
}

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
    let mut used = HashSet::new();
    let mut questions = Vec::new();
    let mut fields = Vec::new();
    for (key, prop) in properties {
        let kind = prop["type"].as_str().unwrap_or("string");
        if !matches!(kind, "string" | "number" | "integer" | "boolean" | "array") {
            return Err(format!("Unsupported field type: {kind}"));
        }
        let title = prop["title"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(key);
        let base = prop["description"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| prop["title"].as_str().filter(|s| !s.is_empty()))
            .or_else(|| params["message"].as_str().filter(|s| !s.is_empty()))
            .unwrap_or(key);
        let mut question = base.to_string();
        let mut suffix = 1;
        while !used.insert(question.clone()) {
            question = format!("{base} ({key}: {suffix})");
            suffix += 1;
        }
        let options = if kind == "boolean" {
            vec![
                QuestionOption {
                    label: "Yes".into(),
                    description: "true".into(),
                },
                QuestionOption {
                    label: "No".into(),
                    description: "false".into(),
                },
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
                    choices.push(QuestionOption {
                        label: label.into(),
                        description: source["enumNames"][i].as_str().unwrap_or("").into(),
                    });
                }
            } else if let Some(values) = source
                .get("oneOf")
                .or_else(|| source.get("anyOf"))
                .and_then(Value::as_array)
            {
                for value in values {
                    let label = value["const"]
                        .as_str()
                        .ok_or("Options must have string const values")?;
                    choices.push(QuestionOption {
                        label: label.into(),
                        description: value["title"]
                            .as_str()
                            .or_else(|| value["description"].as_str())
                            .unwrap_or("")
                            .into(),
                    });
                }
            } else if kind != "array"
                && let Some(default) = prop.get("default")
            {
                choices.push(QuestionOption {
                    label: default
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| default.to_string()),
                    description: String::new(),
                });
            }
            choices
        };
        fields.push(ElicitationFieldKind {
            key: key.clone(),
            question: question.clone(),
            kind: kind.into(),
            options: options.iter().map(|o| o.label.clone()).collect(),
        });
        questions.push(QuestionSpec {
            question,
            header: if title.is_empty() {
                "Question".into()
            } else {
                title.into()
            },
            options,
            multi_select: kind == "array",
        });
    }
    Ok((questions, fields))
}

pub fn build_elicitation_content(fields: &[ElicitationFieldKind], answers: &Value) -> Value {
    let mut content = Map::new();
    for field in fields {
        let Some(answer) = answers.get(&field.question).and_then(Value::as_str) else {
            continue;
        };
        let value = match field.kind.as_str() {
            "array" => {
                // A single enum value can itself contain the PromptCard delimiter.
                let mut values = Vec::new();
                let mut remaining = answer;
                while !remaining.is_empty() {
                    let matched = field
                        .options
                        .iter()
                        .map(String::as_str)
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
                    values.push(value);
                    remaining = tail;
                }
                Value::Array(
                    values
                        .into_iter()
                        .map(|s| Value::String(s.into()))
                        .collect(),
                )
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
            _ => Value::String(answer.into()),
        };
        content.insert(field.key.clone(), value);
    }
    Value::Object(content)
}
