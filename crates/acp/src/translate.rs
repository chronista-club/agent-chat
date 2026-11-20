//! ACP `session/update` → [`ConversationEvent`] 翻訳。
//!
//! VP の `conversation/acp_translate.rs`（nightly `504859ba`）から移した。grok / opencode / agy で
//! 共有する（ACP は engine 非依存）。lifecycle（SessionInit / TurnCompleted / Error / permission）は
//! host 側が扱い、本 module は **`session/update` notification の update object だけ**を純翻訳する。
//!
//! 入力は SDK の型ではなく生の JSON。engine が標準に無い key（agy の `name` / `args` 等）や
//! 知らない種類（x.ai 拡張等）を出しても読み続けるため。
//!
//! ## 方針（VP design 42 §1 / 88 §1 の実測 wire + ACP 標準形で固定）
//!
//! | update.sessionUpdate | ConversationEvent |
//! |----------------------|-------------|
//! | `agent_message_chunk`（content.type=text） | `MessageChunk`（実測） |
//! | `agent_thought_chunk` | `ThoughtChunk`（実測） |
//! | `user_message_chunk` | `UserMessage`。会話中に host が出した prompt の写しを落とすのは host の仕事 |
//! | `tool_call` | `ToolCall`（ACP 標準形 {toolCallId, title, kind, rawInput}）。status が completed / failed なら `ToolCallUpdate` も続ける（`session/load` の流れはこの形、agy 1.5.11 実測） |
//! | `tool_call_update` | `ToolCallUpdate`（status=failed → is_error。started 未観測なら ToolCall 補完。`content` は text / diff 両対応） |
//! | `plan` | `Plan`（{entries: [{content, status}]}） |
//! | その他（usage_update / session_info_update / available_commands_update / x.ai 拡張等） | 無視（メタ情報は host 側で処理、知らない通知は読み飛ばす） |

use std::collections::HashSet;

use agent_chat_protocol::{ConversationEvent, PlanEntry};

/// content block（{type:"text", text}）から text を取り出す（それ以外の type は無視）。
fn content_text(content: &serde_json::Value) -> Option<String> {
    match content.get("type").and_then(|v| v.as_str()) {
        Some("text") => content
            .get("text")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// `tool_call_update` の content 配列 1 要素をテキスト化する。
///
/// - `{type: "content", content: {type: "text", text}}`: 標準 ACP テキスト出力
/// - `{type: "diff", path, oldText?, newText?}`: agy 等の編集系 tool 結果（VP design 88 §1 実測）
fn tool_result_block_text(block: &serde_json::Value) -> Option<String> {
    match block.get("type").and_then(|v| v.as_str()) {
        Some("content") => block.get("content").and_then(content_text),
        Some("diff") => {
            let path = block.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let old_text = block
                .get("oldText")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let new_text = block
                .get("newText")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            match (path.is_empty(), old_text, new_text) {
                (true, None, None) => None,
                (false, None, None) => Some(path.to_string()),
                (true, None, Some(new_s)) => Some(new_s.to_string()),
                (false, None, Some(new_s)) => Some(format!("{path}\n{new_s}")),
                (true, Some(old_s), None) => Some(format!("--- old\n{old_s}")),
                (false, Some(old_s), None) => Some(format!("{path}\n--- old\n{old_s}")),
                (true, Some(old_s), Some(new_s)) => {
                    Some(format!("--- old\n{old_s}\n+++ new\n{new_s}"))
                }
                (false, Some(old_s), Some(new_s)) => {
                    Some(format!("{path}\n--- old\n{old_s}\n+++ new\n{new_s}"))
                }
            }
        }
        _ => block.get("content").and_then(content_text),
    }
}

/// tool の結果（content 配列）を本文にする。text と diff の block を改行でつなぐ。
/// 本文にできる block が無ければ status をそのまま出す。
fn tool_result(update: &serde_json::Value, id: &str, status: &str) -> ConversationEvent {
    let content = update
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(tool_result_block_text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    ConversationEvent::ToolCallUpdate {
        tool_use_id: id.to_string(),
        content: if content.is_empty() {
            status.to_string()
        } else {
            content
        },
        is_error: status == "failed",
    }
}

fn is_terminal(status: &str) -> bool {
    status == "completed" || status == "failed"
}

/// ACP `session/update` の update 層翻訳器（1 host = 1 個、turn を跨いで生存）。
#[derive(Debug, Default)]
pub struct AcpTranslator {
    /// `tool_call` で ToolCall を emit 済みの toolCallId（update 側の補完判定）。
    started_tools: HashSet<String>,
    /// 直前の `user_message_chunk` の `messageId`。会話の中身の update が挟まったら消す。
    last_user_message_id: Option<String>,
}

/// `user_message_chunk` 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserChunk {
    pub text: String,
    /// 直前の chunk と同じ `messageId` = 同じ発話の続き。
    ///
    /// `messageId` を付ける engine（opencode 2.3.0）でだけ true になる。付けない engine
    /// （agy 1.5.11）では、続きか別の発話かを見分けられないので、いつも false（別の発話）。
    pub continues: bool,
}

impl AcpTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    /// `user_message_chunk`（text）なら、その本文と「直前の chunk の続きか」を返す。
    /// 履歴で 1 つの発話にまとめたい呼び手は、[`Self::ingest`] の前にこちらを呼ぶ。
    pub fn ingest_user_chunk(&mut self, update: &serde_json::Value) -> Option<UserChunk> {
        if update.get("sessionUpdate").and_then(|v| v.as_str()) != Some("user_message_chunk") {
            return None;
        }
        let id = update
            .get("messageId")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let continues = id.is_some() && id == self.last_user_message_id;
        self.last_user_message_id = id;
        let text = update.get("content").and_then(content_text)?;
        Some(UserChunk { text, continues })
    }

    /// `session/update` の params.update 1 件を食わせ、0 個以上の [`ConversationEvent`] を得る。
    /// 未知 variant は無条件に空（寛容）。
    pub fn ingest(&mut self, update: &serde_json::Value) -> Vec<ConversationEvent> {
        let kind = update
            .get("sessionUpdate")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // 会話の中身が挟まったら、次の user の chunk は続きにしない。
        // usage / session_info などの状態の通知は挟まっても続きのまま。
        if matches!(
            kind,
            "agent_message_chunk"
                | "agent_thought_chunk"
                | "tool_call"
                | "tool_call_update"
                | "plan"
        ) {
            self.last_user_message_id = None;
        }
        match kind {
            "agent_message_chunk" => update
                .get("content")
                .and_then(content_text)
                .map(|text| vec![ConversationEvent::MessageChunk { text }])
                .unwrap_or_default(),
            "agent_thought_chunk" => update
                .get("content")
                .and_then(content_text)
                .map(|text| vec![ConversationEvent::ThoughtChunk { text }])
                .unwrap_or_default(),
            "user_message_chunk" => self
                .ingest_user_chunk(update)
                .map(|chunk| vec![ConversationEvent::UserMessage { text: chunk.text }])
                .unwrap_or_default(),
            "tool_call" => {
                let id = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("");
                let mut out = vec![ConversationEvent::ToolCall {
                    id: id.clone(),
                    name: update
                        .get("title")
                        .and_then(|v| v.as_str())
                        .or_else(|| update.get("kind").and_then(|v| v.as_str()))
                        .unwrap_or("tool")
                        .to_string(),
                    input: update.get("rawInput").cloned().unwrap_or_default(),
                }];
                if is_terminal(status) {
                    // 結果まで入った 1 件（load の流れ）。終わりの update は来ない
                    out.push(tool_result(update, &id, status));
                } else {
                    self.started_tools.insert(id);
                }
                out
            }
            "tool_call_update" => {
                let id = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("");
                // in_progress 等の中間 update は流さない（終端だけ表示 — 頻度を抑える）。
                if !is_terminal(status) {
                    return Vec::new();
                }
                let mut out = Vec::new();
                if !self.started_tools.remove(id) {
                    out.push(ConversationEvent::ToolCall {
                        id: id.to_string(),
                        name: update
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool")
                            .to_string(),
                        input: update.get("rawInput").cloned().unwrap_or_default(),
                    });
                }
                out.push(tool_result(update, id, status));
                out
            }
            "plan" => {
                let entries = update
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| {
                                let content = e.get("content").and_then(|v| v.as_str())?;
                                Some(PlanEntry {
                                    content: content.to_string(),
                                    status: e
                                        .get("status")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("pending")
                                        .to_string(),
                                    active_form: None,
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if entries.is_empty() {
                    Vec::new()
                } else {
                    vec![ConversationEvent::Plan { entries }]
                }
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VP design 42 §1 の実測 wire（grok 0.2.64）そのままの update で主経路を固定。
    #[test]
    fn real_wire_message_and_thought_chunks() {
        let mut tr = AcpTranslator::new();
        let msg: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"gro"}}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&msg),
            vec![ConversationEvent::MessageChunk { text: "gro".into() }]
        );
        let thought: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"考え中"}}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&thought),
            vec![ConversationEvent::ThoughtChunk {
                text: "考え中".into()
            }]
        );
    }

    /// tool_call → tool_call_update（completed / failed / 中間 in_progress の抑止 / 補完）。
    #[test]
    fn tool_call_lifecycle_maps_and_backfills() {
        let mut tr = AcpTranslator::new();
        let call: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call","toolCallId":"t1","title":"shell","kind":"execute","rawInput":{"command":"ls"}}"#,
        )
        .unwrap();
        let ev = tr.ingest(&call);
        assert!(
            matches!(&ev[0], ConversationEvent::ToolCall { id, name, .. } if id == "t1" && name == "shell")
        );

        // 中間 update は無視
        let progress: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"in_progress"}"#,
        )
        .unwrap();
        assert_eq!(tr.ingest(&progress), Vec::new());

        let done: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"file1"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&done),
            vec![ConversationEvent::ToolCallUpdate {
                tool_use_id: "t1".into(),
                content: "file1".into(),
                is_error: false,
            }]
        );

        // started 未観測の failed → ToolCall 補完 + is_error
        let failed: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t2","status":"failed","title":"shell"}"#,
        )
        .unwrap();
        let ev = tr.ingest(&failed);
        assert_eq!(ev.len(), 2);
        assert!(matches!(
            &ev[1],
            ConversationEvent::ToolCallUpdate { is_error: true, .. }
        ));
    }

    /// plan 変換 + 未知 variant の寛容性（実測の available_commands_update / x.ai 拡張形）。
    #[test]
    fn plan_maps_and_unknown_variants_ignored() {
        let mut tr = AcpTranslator::new();
        let plan: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"plan","entries":[{"content":"調査する","status":"in_progress"},{"content":"実装する","status":"pending"}]}"#,
        )
        .unwrap();
        let ev = tr.ingest(&plan);
        assert!(matches!(&ev[0], ConversationEvent::Plan { entries } if entries.len() == 2));

        let cmds: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"compact"}]}"#,
        )
        .unwrap();
        assert_eq!(tr.ingest(&cmds), Vec::new());
        let hook: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"hook_execution","event_name":"stop","runs":[]}"#,
        )
        .unwrap();
        assert_eq!(tr.ingest(&hook), Vec::new());
    }

    /// VP design 88 §1 の実測 wire（agy 1.5.9）: write_file の diff 結果、user の発話、
    /// 無視する update（usage_update / session_info_update）。
    #[test]
    fn agy_real_wire_diff_tool_result_and_ignored_updates() {
        let mut tr = AcpTranslator::new();
        // user_message_chunk は user の発話。会話中の写しを落とすかは host が決める
        let user_echo: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Create hello.txt"}}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&user_echo),
            vec![ConversationEvent::UserMessage {
                text: "Create hello.txt".into()
            }]
        );

        let usage: serde_json::Value =
            serde_json::from_str(r#"{"sessionUpdate":"usage_update","used":9461,"size":1048576}"#)
                .unwrap();
        assert_eq!(tr.ingest(&usage), Vec::new());

        let info: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"session_info_update","title":"Create Hello Txt File"}"#,
        )
        .unwrap();
        assert_eq!(tr.ingest(&info), Vec::new());

        // write_file tool_call → tool_call_update (content type="diff")
        let call: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call","toolCallId":"call_7800946240387568052","name":"write_file","status":"in_progress","kind":"edit","title":"File hello.txt","locations":[{"path":"/tmp/hello.txt"}],"rawInput":{"Content":"hi","Overwrite":true,"Path":"/tmp/hello.txt"}}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&call),
            vec![ConversationEvent::ToolCall {
                id: "call_7800946240387568052".into(),
                name: "File hello.txt".into(),
                input: serde_json::json!({"Content":"hi","Overwrite":true,"Path":"/tmp/hello.txt"}),
            }]
        );

        let update: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"call_7800946240387568052","name":"write_file","status":"completed","kind":"edit","title":"File hello.txt","content":[{"type":"diff","path":"/tmp/hello.txt","newText":"hi"}]}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&update),
            vec![ConversationEvent::ToolCallUpdate {
                tool_use_id: "call_7800946240387568052".into(),
                content: "/tmp/hello.txt\nhi".into(),
                is_error: false,
            }]
        );
    }

    /// `session/load` の流れ（agy 1.5.11 実測の形）: `tool_call` 1 件に status と結果まで入り、
    /// `tool_call_update` は来ない。`ToolCall` と `ToolCallUpdate` の 2 つにする。
    #[test]
    fn terminal_tool_call_carries_its_result() {
        let mut tr = AcpTranslator::new();
        let done: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call","toolCallId":"c1","name":"run_command","status":"completed","kind":"execute","title":"ls","args":{"x":1},"rawInput":{"CommandLine":"ls"},"rawOutput":{"out":"a"},"content":[{"type":"content","content":{"type":"text","text":"a.txt"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&done),
            vec![
                ConversationEvent::ToolCall {
                    id: "c1".into(),
                    name: "ls".into(),
                    input: serde_json::json!({"CommandLine":"ls"}),
                },
                ConversationEvent::ToolCallUpdate {
                    tool_use_id: "c1".into(),
                    content: "a.txt".into(),
                    is_error: false,
                },
            ]
        );

        // content が無い（locations だけ）: 本文は status、failed は is_error
        let failed: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call","toolCallId":"c2","status":"failed","kind":"read","title":"View x","locations":[{"path":"/x"}],"rawInput":{}}"#,
        )
        .unwrap();
        let ev = tr.ingest(&failed);
        assert_eq!(ev.len(), 2);
        assert_eq!(
            ev[1],
            ConversationEvent::ToolCallUpdate {
                tool_use_id: "c2".into(),
                content: "failed".into(),
                is_error: true,
            }
        );

        // resource_link の block は本文にしない（text と diff だけ）
        let link: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call","toolCallId":"c3","status":"completed","title":"gen","rawInput":{},"content":[{"type":"content","content":{"type":"resource_link","uri":"file:///a.png","name":"a.png"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&link)[1],
            ConversationEvent::ToolCallUpdate {
                tool_use_id: "c3".into(),
                content: "completed".into(),
                is_error: false,
            }
        );
    }

    /// 会話中の `tool_call`（in_progress）は今までどおり `ToolCall` だけ。終わりは update で来る。
    #[test]
    fn in_progress_tool_call_waits_for_its_update() {
        let mut tr = AcpTranslator::new();
        let call: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call","toolCallId":"t9","status":"in_progress","title":"shell","rawInput":{}}"#,
        )
        .unwrap();
        assert_eq!(tr.ingest(&call).len(), 1);
        let done: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t9","status":"completed"}"#,
        )
        .unwrap();
        assert_eq!(
            tr.ingest(&done).len(),
            1,
            "開始済みなので ToolCall は補わない"
        );
    }

    /// 画像など text でない user の発話は、今は出さない（`UserMessage` は text だけ）。
    #[test]
    fn non_text_user_chunk_is_dropped() {
        let mut tr = AcpTranslator::new();
        let img: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"image","mimeType":"image/png","data":"AAAA"}}"#,
        )
        .unwrap();
        assert_eq!(tr.ingest(&img), Vec::new());
    }

    /// opencode 2.3.0 は `user_message_chunk` に `messageId` を付ける（2026-11-20 実測）。
    /// 同じ id が続いたら、同じ発話の続き。id が無い（agy）・違う・間に他の update が挟まるなら別の発話。
    #[test]
    fn user_chunks_with_same_message_id_continue_one_message() {
        let chunk = |id: Option<&str>, text: &str| {
            let mut u = serde_json::json!({
                "sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": text},
            });
            if let Some(id) = id {
                u["messageId"] = id.into();
            }
            u
        };
        let mut tr = AcpTranslator::new();
        let ingest = |tr: &mut AcpTranslator, u: &serde_json::Value| {
            let c = tr.ingest_user_chunk(u).expect("user の発話");
            (c.text, c.continues)
        };
        assert_eq!(
            ingest(&mut tr, &chunk(Some("m1"), "he")),
            ("he".into(), false)
        );
        assert_eq!(
            ingest(&mut tr, &chunk(Some("m1"), "llo")),
            ("llo".into(), true)
        );
        assert_eq!(
            ingest(&mut tr, &chunk(Some("m2"), "x")),
            ("x".into(), false)
        );
        assert_eq!(ingest(&mut tr, &chunk(None, "y")), ("y".into(), false));
        assert_eq!(
            ingest(&mut tr, &chunk(None, "z")),
            ("z".into(), false),
            "id が無ければ別の発話"
        );

        // 間に他の update が挟まったら、同じ id でも続きにしない
        assert_eq!(
            ingest(&mut tr, &chunk(Some("m3"), "a")),
            ("a".into(), false)
        );
        let answer: serde_json::Value = serde_json::from_str(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}"#,
        )
        .unwrap();
        tr.ingest(&answer);
        assert_eq!(
            ingest(&mut tr, &chunk(Some("m3"), "b")),
            ("b".into(), false)
        );

        // user の発話でない update は None。text でない発話も None
        assert!(tr.ingest_user_chunk(&answer).is_none());
        let image = serde_json::json!({"sessionUpdate": "user_message_chunk", "messageId": "m4",
            "content": {"type": "image", "mimeType": "image/png", "data": "AAAA"}});
        assert!(tr.ingest_user_chunk(&image).is_none());
    }
}
