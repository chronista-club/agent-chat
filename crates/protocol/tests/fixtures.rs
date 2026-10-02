//! ConversationEvent の Rust → TS 契約 fixture（棚卸し 項目 8 / 8-1）。
//!
//! SSOT は `agent-chat/crates/protocol/src/event.rs`。vp-app の Rust は event を
//! `serde_json::Value` で素通しし、TS 型は 8-2 で ts-rs が `webview/src/generated/ConversationEvent.ts`
//! に生成する。ここでは **Rust が実際に serialize する形（送信形）** を全 variant について
//! TS の literal file に書き出し、webview の `tsc --noEmit`（`satisfies`）と vitest が
//! 生成型との一致（= ts-rs 注釈の付け忘れ・付け間違い）を検査する。
//!
//! - 送信形 = `skip_serializing_if` で省略される optional は **無い / 有る** の両方を載せる。
//!   `#[serde(default)]` だけの field（`is_error` / `multi_select` / `description`）は
//!   常に serialize されるので TS 側も必須で受ける
//! - 受理形（古い payload を Rust が読む側 = `replay_log.rs` が過去版の JSONL を読む）は対象外:
//!   TS への送り手は Rust だけで、読んだ event も再 serialize されてから TS に届く（default が埋まる）
//! - 生成物は commit する。CI の `git diff --exit-code` が drift を検出する
//!   （codegen test 自体は「書き換えて成功」なので、再生成の成功と drift 検出は別）
//!
//! 再生成: `cargo test -p agent-chat-protocol --test fixtures`

use std::collections::HashMap;
use std::path::Path;

use agent_chat_protocol::event::{
    ConversationEvent, PlanEntry, QuestionOption, QuestionSpec, SubagentRole,
};

/// TS 側の union が持つべき kind。variant を足したらここも足す
/// （fixture の網羅 assert が落ちて気付く）。
const EXPECTED_KINDS: [&str; 30] = [
    "codex_permission_state",
    "codex_permission_applied",
    "codex_requests",
    "codex_response",
    "notice",
    "mcp_status",
    "permission_cancelled",
    "permission_mode",
    "codex_queue",
    "codex_message",
    "codex_interactions",
    "codex_interaction_result",
    "session_init",
    "replay_start",
    "replay_end",
    "codex_config",
    "codex_history",
    "user_message",
    "message_chunk",
    "thought_chunk",
    "tool_call",
    "tool_call_update",
    "subagent_message",
    "plan",
    "turn_completed",
    "now_line",
    "error",
    "engine_exited",
    "question",
    "permission_request",
];

/// 全 variant の代表値。optional の有無 / 空 vec・map / false / 数値 / 任意 JSON を含める。
fn fixtures() -> Vec<(&'static str, ConversationEvent)> {
    let mut events = vec![
        (
            "codex_message",
            ConversationEvent::CodexMessage {
                item_id: "turn/message".into(),
                text: "本文".into(),
                questions: Vec::new(),
                append: false,
            },
        ),
        (
            "codex_interactions",
            ConversationEvent::CodexInteractions {
                requests: vec![agent_chat_protocol::event::CodexInteraction {
                    elicitation: None,
                    cancel_on_deny: None,
                    item_id: None,
                    request_id: "codex:fixture:1".into(),
                    kind: "question".into(),
                    title: "質問".into(),
                    details: String::new(),
                    blocking: true,
                    can_accept: true,
                    questions: vec![agent_chat_protocol::event::CodexQuestion {
                        id: "question-id".into(),
                        header: "対象".into(),
                        question: "どちら？".into(),
                        options: vec![QuestionOption {
                            label: "A".into(),
                            description: "候補".into(),
                        }],
                        is_secret: false,
                    }],
                }],
            },
        ),
        (
            "codex_interaction_result",
            ConversationEvent::CodexInteractionResult {
                request_id: "codex:fixture:1".into(),
                error: None,
            },
        ),
        (
            "codex_config",
            ConversationEvent::CodexConfig {
                config: Some(agent_chat_protocol::event::CodexConfigView::default()),
                request_id: None,
                error: None,
            },
        ),
        (
            "codex_queue",
            ConversationEvent::CodexQueue {
                queue: Some(agent_chat_protocol::event::CodexQueueView::default()),
                request_id: None,
                error: None,
            },
        ),
        (
            "codex_history",
            ConversationEvent::CodexHistory {
                thread_id: "codex-thread".into(),
                events: vec![ConversationEvent::UserMessage {
                    text: "Console の会話".into(),
                }],
                user_message_ids: vec!["request-1".into()],
                in_flight: false,
                truncated: true,
            },
        ),
        (
            "session_init_minimal",
            ConversationEvent::SessionInit {
                session_id: "sid-1".into(),
                model: None,
                permission_mode: None,
                cwd: None,
                tools: vec![],
                mcp_servers: vec![],
                slash_commands: vec![],
                command_docs: HashMap::new(),
            },
        ),
        (
            "session_init_full",
            ConversationEvent::SessionInit {
                session_id: "sid-2".into(),
                model: Some("claude-fable-5-1".into()),
                permission_mode: Some("bypassPermissions".into()),
                cwd: Some("/Users/x/repo".into()),
                tools: vec!["Bash".into(), "Read".into()],
                mcp_servers: vec!["vantage-point".into()],
                slash_commands: vec!["compact".into()],
                // HashMap は 1 entry なら出力順が決定的
                command_docs: HashMap::from([("compact".to_string(), "会話を圧縮".to_string())]),
            },
        ),
        ("replay_start", ConversationEvent::ReplayStart),
        (
            "replay_end_in_flight",
            ConversationEvent::ReplayEnd { in_flight: true },
        ),
        (
            "replay_end_idle",
            ConversationEvent::ReplayEnd { in_flight: false },
        ),
        (
            "user_message",
            ConversationEvent::UserMessage {
                text: "こんにちは".into(),
            },
        ),
        (
            "message_chunk",
            ConversationEvent::MessageChunk {
                text: "chunk".into(),
            },
        ),
        (
            "thought_chunk",
            ConversationEvent::ThoughtChunk {
                text: "thinking…".into(),
            },
        ),
        (
            "tool_call",
            ConversationEvent::ToolCall {
                id: "toolu_01".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls", "timeout": 1000, "nested": {"a": [1, 2]}}),
            },
        ),
        (
            "tool_call_update_ok",
            ConversationEvent::ToolCallUpdate {
                tool_use_id: "toolu_01".into(),
                content: "ok".into(),
                is_error: false,
            },
        ),
        (
            "tool_call_update_error",
            ConversationEvent::ToolCallUpdate {
                tool_use_id: "toolu_02".into(),
                content: "boom".into(),
                is_error: true,
            },
        ),
        (
            "subagent_message_text",
            ConversationEvent::SubagentMessage {
                parent_tool_use_id: "toolu_03".into(),
                role: SubagentRole::Text,
                text: "sub".into(),
            },
        ),
        (
            "subagent_message_prompt",
            ConversationEvent::SubagentMessage {
                parent_tool_use_id: "toolu_03".into(),
                role: SubagentRole::Prompt,
                text: "子への指示".into(),
            },
        ),
        (
            "subagent_message_thinking",
            ConversationEvent::SubagentMessage {
                parent_tool_use_id: "toolu_03".into(),
                role: SubagentRole::Thinking,
                text: "子の思考".into(),
            },
        ),
        (
            "plan",
            ConversationEvent::Plan {
                entries: vec![
                    PlanEntry {
                        content: "調べる".into(),
                        status: "completed".into(),
                        active_form: None,
                    },
                    PlanEntry {
                        content: "直す".into(),
                        status: "in_progress".into(),
                        active_form: Some("直している".into()),
                    },
                ],
            },
        ),
        (
            "turn_completed_minimal",
            ConversationEvent::TurnCompleted {
                session_id: "sid-1".into(),
                cost_usd: None,
                context_tokens: None,
                context_window: None,
            },
        ),
        (
            "turn_completed_full",
            ConversationEvent::TurnCompleted {
                session_id: "sid-2".into(),
                cost_usd: Some(0.0123),
                // u64 だが JSON では number。TS は number で受ける（2^53 未満の運用値）
                context_tokens: Some(12_345),
                context_window: Some(200_000),
            },
        ),
        (
            "now_line",
            ConversationEvent::NowLine {
                text: "panic 箇所を特定中".into(),
            },
        ),
        (
            "error",
            ConversationEvent::Error {
                message: "engine error".into(),
            },
        ),
        (
            "engine_exited",
            ConversationEvent::EngineExited {
                message: "exit 0".into(),
            },
        ),
        (
            "question",
            ConversationEvent::Question {
                request_id: "req-1".into(),
                questions: vec![QuestionSpec {
                    question: "どちら？".into(),
                    header: "選択".into(),
                    options: vec![
                        QuestionOption {
                            label: "A".into(),
                            description: "説明あり".into(),
                        },
                        QuestionOption {
                            label: "B".into(),
                            description: String::new(),
                        },
                    ],
                    multi_select: false,
                }],
            },
        ),
        (
            "permission_request",
            ConversationEvent::PermissionRequest {
                request_id: "req-2".into(),
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "rm -rf /tmp/x"}),
                display_name: None,
                description: None,
                tool_use_id: None,
                suggestions: Vec::new(),
            },
        ),
    ];
    for (name, payload) in [
        (
            "codex_permission_state",
            serde_json::json!({"kind":"codex_permission_state","conversation_key":"a","selected":"standard","effective":null,"error":null}),
        ),
        (
            "codex_permission_applied",
            serde_json::json!({"kind":"codex_permission_applied","mode":"standard"}),
        ),
        (
            "codex_requests",
            serde_json::json!({"kind":"codex_requests","requests":[{"request_id":"a:1","kind":"mcp_approval","title":"確認","detail":{},"questions":[],"can_accept":true,"can_remember":true}]}),
        ),
        (
            "codex_response",
            serde_json::json!({"kind":"codex_response","request_id":"a:1","error":null}),
        ),
        (
            "notice",
            serde_json::json!({"kind":"notice","message":"通知"}),
        ),
        (
            "mcp_status",
            serde_json::json!({"kind":"mcp_status","server":"gfp-estimate","status":"connected"}),
        ),
        (
            "permission_cancelled",
            serde_json::json!({"kind":"permission_cancelled","request_id":"a:1"}),
        ),
        (
            "permission_mode",
            serde_json::json!({"kind":"permission_mode","mode":"default"}),
        ),
    ] {
        events.push((name, serde_json::from_value(payload).unwrap()));
    }
    events
}

fn render() -> String {
    let mut out = String::new();
    out.push_str("// 生成物 — 編集しない。SSOT は agent-chat/crates/protocol/src/event.rs、\n");
    out.push_str("// 再生成は `cargo test -p agent-chat-protocol --test fixtures`。\n");
    out.push_str("// Rust が実際に serialize した ConversationEvent（送信形）。`satisfies` で ts-rs 生成型と\n");
    out.push_str("// 突き合わせる（tsc --noEmit）= field 名 / kind / 型 / 「TS が Rust より厳しい」向きの必須性を型検査で止める。\n");
    out.push_str("// 「TS が緩い」向き（Rust が常に出す field を TS が ? にする）は型では通るので vitest 側で固定する。\n");
    out.push_str("import type { ConversationEvent as EngineConversationEvent } from './ConversationEvent'\n\n");
    out.push_str("export const CONVERSATION_EVENT_FIXTURES = {\n");
    for (name, ev) in fixtures() {
        let json = serde_json::to_string_pretty(&ev).expect("serialize");
        let indented = json
            .lines()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
            .trim_start()
            .to_string();
        out.push_str(&format!("  {name}: {indented},\n"));
    }
    out.push_str("} satisfies Record<string, EngineConversationEvent>\n");
    out
}

fn write_if_changed(path: &Path, content: &str) {
    if std::fs::read_to_string(path).is_ok_and(|cur| cur == content) {
        return;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("生成先ディレクトリ作成失敗");
    }
    std::fs::write(path, content)
        .unwrap_or_else(|e| panic!("生成物の書き込み失敗 {}: {e}", path.display()));
}

/// 全 variant が fixture に 1 つ以上ある（variant 追加の取りこぼしを Rust 側で止める）。
#[test]
fn fixtures_cover_every_kind() {
    let kinds: std::collections::BTreeSet<String> = fixtures()
        .iter()
        .map(|(_, ev)| {
            serde_json::to_value(ev).expect("serialize")["kind"]
                .as_str()
                .expect("kind is a string")
                .to_string()
        })
        .collect();
    let expected: std::collections::BTreeSet<String> =
        EXPECTED_KINDS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        kinds, expected,
        "fixture の kind 集合が EXPECTED_KINDS と一致すること"
    );
}

/// serialize → deserialize → serialize が同じ JSON に戻る（送信形は自己整合）。
#[test]
fn fixtures_roundtrip_through_serde() {
    for (name, ev) in fixtures() {
        let json = serde_json::to_value(&ev).expect("serialize");
        let back: ConversationEvent = serde_json::from_value(json.clone()).expect(name);
        assert_eq!(back, ev, "{name}: deserialize が値を保つ");
        assert_eq!(
            serde_json::to_value(&back).expect("serialize"),
            json,
            "{name}: 再 serialize が同じ JSON"
        );
    }
}

/// TS literal file を生成する（決定的、内容不変なら書かない）。
#[test]
fn generate_ts_fixture_file() {
    let a = render();
    let b = render();
    assert_eq!(a, b, "生成は決定的であること");
    let out = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/chat/src/generated/ConversationEventFixtures.ts");
    write_if_changed(&out, &a);
}
