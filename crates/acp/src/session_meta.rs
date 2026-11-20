//! session の付随情報の抽出（model / mode / slash command / context の使用量）。
//!
//! VP の `conversation/acp_host.rs`（nightly `504859ba`）の純関数を移した。入力は生の JSON。

use std::collections::HashMap;

/// `session/new` / `session/load` の result から `(model, permission_mode)` を抽出する（純関数）。
///
/// - `model`: `models.currentModelId` を優先し、無ければ `configOptions` の `id=="model"` の `currentValue`。
/// - `permission_mode`: `modes.currentModeId`。
pub fn extract_session_meta(result: &serde_json::Value) -> (Option<String>, Option<String>) {
    let model = result
        .pointer("/models/currentModelId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            result
                .get("configOptions")
                .and_then(|v| v.as_array())?
                .iter()
                .find(|opt| opt.get("id").and_then(|v| v.as_str()) == Some("model"))?
                .get("currentValue")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    let permission_mode = result
        .pointer("/modes/currentModeId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (model, permission_mode)
}

/// `session/update` の `available_commands_update` から `(slash_commands, command_docs)` を抽出する（純関数）。
pub fn extract_available_commands(
    update: &serde_json::Value,
) -> Option<(Vec<String>, HashMap<String, String>)> {
    if update.get("sessionUpdate").and_then(|v| v.as_str()) != Some("available_commands_update") {
        return None;
    }
    let arr = update.get("availableCommands")?.as_array()?;
    let mut commands = Vec::with_capacity(arr.len());
    let mut docs = HashMap::new();
    for item in arr {
        let Some(name) = item
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        commands.push(name.to_string());
        if let Some(desc) = item
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            docs.insert(name.to_string(), desc.to_string());
        }
    }
    Some((commands, docs))
}

/// `session/update` の `current_mode_update` から mode id を抽出する（純関数、実測 agy 1.5.9）。
pub fn extract_current_mode(update: &serde_json::Value) -> Option<String> {
    if update.get("sessionUpdate").and_then(|v| v.as_str()) != Some("current_mode_update") {
        return None;
    }
    update
        .get("currentModeId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `session/update` の `usage_update` から `(used_tokens, window_size)` を抽出する（純関数）。
pub fn extract_usage_update(update: &serde_json::Value) -> Option<(u64, u64)> {
    if update.get("sessionUpdate").and_then(|v| v.as_str()) != Some("usage_update") {
        return None;
    }
    let used = update.get("used")?.as_u64()?;
    let size = update.get("size")?.as_u64().filter(|&s| s > 0)?;
    Some((used, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VP design 88 §3: agy acp の実測応答から model / mode / available_commands / usage を抽出する。
    #[test]
    fn extract_session_meta_and_updates_from_agy_real_wire() {
        let session_new_result: serde_json::Value = serde_json::from_str(
            r#"{
                "sessionId": "29fa9ed4-cd23-429f-9a63-a5d490b0d9b3",
                "models": { "currentModelId": "gemini-4-argon-medium" },
                "modes": { "currentModeId": "default" }
            }"#,
        )
        .unwrap();
        assert_eq!(
            extract_session_meta(&session_new_result),
            (
                Some("gemini-4-argon-medium".to_string()),
                Some("default".to_string())
            )
        );

        let cmds_update: serde_json::Value = serde_json::from_str(
            r#"{
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    {"name": "goal", "description": "Run until finished."},
                    {"name": "compact", "description": "Compact history."}
                ]
            }"#,
        )
        .unwrap();
        let (cmds, docs) = extract_available_commands(&cmds_update).expect("commands");
        assert_eq!(cmds, vec!["goal", "compact"]);
        assert_eq!(
            docs.get("goal").map(String::as_str),
            Some("Run until finished.")
        );

        let usage_update: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate": "usage_update", "used": 9546, "size": 1048576}"#,
        )
        .unwrap();
        assert_eq!(extract_usage_update(&usage_update), Some((9546, 1_048_576)));
    }

    #[test]
    fn model_falls_back_to_config_option_and_mode_update_is_read() {
        let result = serde_json::json!({
            "configOptions": [{"id": "model", "type": "select", "currentValue": "gemini-3.8-flash"}]
        });
        assert_eq!(
            extract_session_meta(&result),
            (Some("gemini-3.8-flash".to_string()), None)
        );
        let mode =
            serde_json::json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"});
        assert_eq!(extract_current_mode(&mode), Some("plan".to_string()));
        let zero = serde_json::json!({"sessionUpdate": "usage_update", "used": 1, "size": 0});
        assert_eq!(
            extract_usage_update(&zero),
            None,
            "window が 0 なら載せない"
        );
    }
}
