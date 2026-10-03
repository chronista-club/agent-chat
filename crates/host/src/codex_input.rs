//! Validated native input and queue contracts. Hosts own transport and permissions.
use agent_chat_protocol::event::{CodexQueueView, CodexQueuedInput};
use base64::Engine;
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, String>;
pub fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 32768)
        .ok_or_else(|| format!("入力操作の {key} が不正です"))
}

pub fn input(text: &str, images: &Value) -> Result<Value> {
    if text.len() > 32768 {
        return Err("入力が長すぎます".into());
    }
    let mut input = Vec::new();
    if !text.trim().is_empty() {
        input.push(json!({"type":"text","text":text,"text_elements":[]}));
    }
    let empty = Vec::new();
    let images = if images.is_null() {
        &empty
    } else {
        images.as_array().ok_or("画像の形式が不正です")?
    };
    for image in images {
        let media = field(image, "media_type")?;
        if !matches!(
            media,
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ) {
            return Err("未対応の画像形式です".into());
        }
        let data = image["data"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("画像データが空です")?;
        const LIMIT: usize = 5 * 1024 * 1024;
        if data.len() > LIMIT.div_ceil(3) * 4 {
            return Err("画像は 1 枚 5 MiB 以下にしてください".into());
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| "画像の base64 データが不正です")?;
        if decoded.is_empty() || decoded.len() > LIMIT {
            return Err("画像は 1 枚 5 MiB 以下にしてください".into());
        }
        input.push(json!({"type":"image","url":format!("data:{media};base64,{data}")}));
    }
    if input.is_empty() {
        return Err("入力が空です".into());
    }
    Ok(json!(input))
}

pub fn operation(
    queue: &CodexQueueView,
    busy: bool,
    action: &Value,
) -> Result<(&'static str, Value)> {
    if !queue.ready {
        return Err("待機一覧の更新完了後に操作してください".into());
    }
    let kind = field(action, "kind")?;
    let mut params = json!({"threadId":queue.thread_id});
    let method = match kind {
        "add" | "steer" => {
            params["input"] = input(
                action["text"].as_str().unwrap_or_default(),
                &action["images"],
            )?;
            params["clientUserMessageId"] = field(action, "client_id")?.into();
            if kind == "steer" {
                let turn = field(action, "turn_id")?;
                if !busy || queue.turn_id.as_deref() != Some(turn) {
                    return Err("宛先ターンが変更されています".into());
                }
                params["expectedTurnId"] = turn.into();
                "turn/steer"
            } else {
                "thread/queue/add"
            }
        }
        "update" | "delete" | "start" => {
            let id = field(action, "id")?;
            let item = queue
                .items
                .iter()
                .find(|i| i.id == id)
                .ok_or("待機項目は既に開始または削除されています")?;
            params["queuedSubmissionId"] = id.into();
            match kind {
                "update" => {
                    if !item.editable {
                        return Err("この形式の待機入力は編集できません".into());
                    }
                    params["input"] = input(field(action, "text")?, &action["images"])?;
                    "thread/queue/update"
                }
                "start" => {
                    if busy {
                        return Err("現在の応答の完了後に再開してください".into());
                    }
                    "thread/queue/start"
                }
                _ => "thread/queue/delete",
            }
        }
        "reorder" => {
            let ids = action["ids"].as_array().ok_or("順序が不正です")?;
            let actual: std::collections::HashSet<_> =
                ids.iter().filter_map(Value::as_str).collect();
            let expected: std::collections::HashSet<_> =
                queue.items.iter().map(|i| i.id.as_str()).collect();
            if ids.len() != actual.len() || actual != expected {
                return Err("待機一覧が変更されています".into());
            }
            params["queuedSubmissionIds"] = action["ids"].clone();
            "thread/queue/reorder"
        }
        _ => return Err("未対応の入力操作です".into()),
    };
    Ok((method, params))
}

pub fn confirmed(method: &str, result: &Value, action: &Value) -> bool {
    match method {
        "turn/steer" => result["turnId"]
            .as_str()
            .is_some_and(|id| Some(id) == action["turn_id"].as_str()),
        "thread/queue/start" => result["turn"]["id"].as_str().is_some_and(|s| !s.is_empty()),
        "thread/queue/add" | "thread/queue/update" => result["queuedSubmission"]["id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "thread/queue/delete" => result["deleted"] == true,
        "thread/queue/reorder" => result.is_object(),
        _ => false,
    }
}

pub fn page(result: &Value) -> Result<(Vec<CodexQueuedInput>, Option<String>)> {
    let mut items = Vec::new();
    for row in result["data"]
        .as_array()
        .ok_or("待機一覧の形式が不正です")?
    {
        let input = row["input"].as_array().ok_or("待機入力の形式が不正です")?;
        items.push(CodexQueuedInput {
            id: field(row, "id")?.into(),
            client_id: field(row, "clientUserMessageId")?.into(),
            editable: !input.is_empty()
                && input.iter().all(|i| {
                    i["type"] == "text"
                        && i["text"].is_string()
                        && i["text_elements"].as_array().is_none_or(Vec::is_empty)
                }),
            text: input
                .iter()
                .map(|i| i["text"].as_str().unwrap_or("［添付・特殊入力］"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }
    let next = match &result["nextCursor"] {
        Value::Null => None,
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => return Err("待機一覧のページ情報が不正です".into()),
    };
    Ok((items, next))
}
