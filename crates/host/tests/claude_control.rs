use agent_chat_host::claude_translate::{
    ClaudeTranslator, control_events, unanswerable_control_request,
};
use agent_chat_protocol::ConversationEvent;

#[test]
fn control_requests_preserve_metadata_without_committing_transcript() {
    let line = r#"{"type":"control_request","request_id":"permission-1","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Shell","description":"Run a check","input":{"command":"cargo check"},"tool_use_id":"tool-1","permission_suggestions":[{"type":"addRules","destination":"session"}]}}"#;
    let events = control_events(line).unwrap();
    let [
        ConversationEvent::PermissionRequest {
            request_id,
            display_name,
            description,
            tool_use_id,
            suggestions,
            ..
        },
    ] = events.as_slice()
    else {
        panic!("permission event expected")
    };
    assert_eq!(request_id, "permission-1");
    assert_eq!(display_name.as_deref(), Some("Shell"));
    assert_eq!(description.as_deref(), Some("Run a check"));
    assert_eq!(tool_use_id.as_deref(), Some("tool-1"));
    assert_eq!(suggestions[0]["destination"], "session");
    assert_eq!(unanswerable_control_request(line), None);
    let out = ClaudeTranslator::new().ingest(line);
    assert!(
        out.events.is_empty(),
        "the process host routes controls separately"
    );
    assert!(!out.commits_transcript);
}

#[test]
fn unsupported_requests_are_identified_for_an_error_response() {
    let line = r#"{"type":"control_request","request_id":"unsupported-1","request":{"subtype":"request_user_dialog"}}"#;
    assert_eq!(
        unanswerable_control_request(line).as_deref(),
        Some("unsupported-1")
    );
    assert_eq!(control_events(line), Some(vec![]));
    assert_eq!(control_events("not json"), None);
}

#[test]
fn cancellation_preserves_request_identity_without_transcript_commit() {
    let line = r#"{"type":"control_cancel_request","request_id":"permission-1"}"#;
    assert_eq!(
        control_events(line),
        Some(vec![ConversationEvent::PermissionCancelled {
            request_id: "permission-1".into()
        }])
    );
    assert!(!ClaudeTranslator::new().ingest(line).commits_transcript);
}
