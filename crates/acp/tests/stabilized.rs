//! Spec mem_1ChUb85e8Zxt8vYkyUonFe: capability and compatibility translation contracts.
use agent_chat_acp::{config::parse_config_options, translate::AcpTranslator};
use agent_chat_protocol::ConversationEvent as E;
use serde_json::json;

#[test]
fn config_ids_categories_and_grouped_options_survive() {
    let options = parse_config_options(&json!([{"configId":"thinking","category":"thought_level","type":"select","currentValue":"high","options":[{"groupId":"levels","options":[{"value":"high","name":"High"}]}]}])).unwrap();
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].id, "thinking");
    assert_eq!(
        serde_json::to_value(&options[0]).unwrap()["category"],
        "thought_level"
    );
    assert_eq!(options[0].choices[0].value, "high");
}

#[test]
fn notices_and_compaction_are_visible() {
    let mut tr = AcpTranslator::new();
    assert_eq!(
        tr.ingest(&json!({"sessionUpdate":"notice","title":"Heads up","description":"Details"})),
        vec![E::Notice {
            message: "Heads up\nDetails".into()
        }]
    );
    for (status, text) in [
        ("in_progress", "コンテキストを圧縮しています…"),
        ("completed", "コンテキストの圧縮が完了しました"),
    ] {
        assert_eq!(
            tr.ingest(&json!({"sessionUpdate":"compaction_update","status":status})),
            vec![E::Notice {
                message: text.into()
            }]
        );
    }
}

#[test]
fn plan_updates_and_removal_reach_the_view() {
    let mut tr = AcpTranslator::new();
    for key in ["items", "entries"] {
        let mut plan = json!({"type":"items"});
        plan[key] = json!([{"content":"Implement","status":"in_progress"}]);
        let events = tr.ingest(&json!({"sessionUpdate":"plan_update", "plan":plan}));
        assert!(
            matches!(&events[..], [E::Plan { entries }] if entries.len()==1 && entries[0].content=="Implement")
        );
    }
    let events = tr.ingest(
        &json!({"sessionUpdate":"plan_update", "plan":{"type":"markdown","content":"# Work"}}),
    );
    assert!(matches!(&events[..], [E::Plan { entries }] if entries[0].content=="# Work"));
    assert_eq!(
        tr.ingest(&json!({"sessionUpdate":"plan_removed"})),
        vec![E::Plan { entries: vec![] }]
    );
}

#[test]
fn full_messages_are_translated() {
    let mut tr = AcpTranslator::new();
    for (kind, expected) in [
        (
            "user_message",
            E::UserMessage {
                text: "hello".into(),
            },
        ),
        (
            "agent_message",
            E::MessageChunk {
                text: "hello".into(),
            },
        ),
        (
            "agent_thought",
            E::ThoughtChunk {
                text: "hello".into(),
            },
        ),
    ] {
        assert_eq!(
            tr.ingest(&json!({"sessionUpdate":kind,"content":[{"type":"text","text":"hello"}]})),
            vec![expected]
        );
    }
}

#[test]
fn tool_updates_start_once_prefer_title_and_finish_cancelled() {
    let mut tr = AcpTranslator::new();
    let events = tr.ingest(&json!({"sessionUpdate":"tool_call_update","toolCallId":"t","name":"read_file","title":"Reading file","status":"in_progress"}));
    // 表示名は title が先（name は title が無い時だけ。translate の単体 test）
    assert!(matches!(&events[..], [E::ToolCall{name,..}] if name=="Reading file"));
    assert!(
        tr.ingest(
            &json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"in_progress"})
        )
        .is_empty()
    );
    let events = tr
        .ingest(&json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"cancelled"}));
    assert!(matches!(&events[..], [E::ToolCallUpdate{content,..}] if content=="cancelled"));
}

#[test]
fn structured_diff_renders_paths_and_patch() {
    let mut tr = AcpTranslator::new();
    let events = tr.ingest(&json!({"sessionUpdate":"tool_call","toolCallId":"t","name":"edit_file","title":"Edit","status":"completed","content":[{"type":"diff","changes":[{"type":"modify","path":"/work/a.rs"}],"patch":{"format":"git_patch","text":"-old\n+new"}}]}));
    assert!(matches!(&events[0], E::ToolCall{name,..} if name=="Edit"));
    assert!(
        matches!(&events[1], E::ToolCallUpdate{content,..} if content.contains("/work/a.rs") && content.contains("-old\n+new"))
    );
}
