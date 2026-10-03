use agent_chat_host::codex_input::{input, operation, page};
use agent_chat_protocol::event::CodexQueueView;
use serde_json::json;

#[test]
fn image_input_validates_and_preserves_payload() {
    let result = input(
        "describe",
        &json!([{"media_type":"image/png","data":"aGVsbG8="}]),
    )
    .unwrap();
    assert_eq!(
        result[1],
        json!({"type":"image","url":"data:image/png;base64,aGVsbG8="})
    );
    assert!(
        input(
            "describe",
            &json!([{"media_type":"image/png","data":"invalid!"}])
        )
        .is_err()
    );
    assert!(
        input(
            "describe",
            &json!([{"media_type":"text/html","data":"aGVsbG8="}])
        )
        .is_err()
    );
    assert!(input("", &json!([])).is_err());
    assert!(input("", &json!([{"media_type":"image/png","data":"aGVsbG8="}])).is_ok());
}

#[test]
fn native_queue_preserves_identity_and_readonly_attachments() {
    let (rows, next) = page(&json!({"data":[{"id":"q","clientUserMessageId":"client","input":[{"type":"image","url":"secret"}]}],"nextCursor":null})).unwrap();
    assert_eq!(rows[0].client_id, "client");
    assert!(!rows[0].editable);
    assert!(!rows[0].text.contains("secret"));
    assert!(next.is_none());
    let queue = CodexQueueView {
        thread_id: "t".into(),
        turn_id: Some("turn".into()),
        ready: true,
        items: rows,
        error: None,
    };
    assert!(
        operation(
            &queue,
            true,
            &json!({"kind":"update","id":"q","text":"edit"})
        )
        .is_err()
    );
    assert!(operation(&queue, true, &json!({"kind":"start","id":"q"})).is_err());
    assert!(operation(&queue, true, &json!({"kind":"reorder","ids":["q","q"]})).is_err());
    let (method, params) = operation(&queue, true, &json!({"kind":"delete","id":"q"})).unwrap();
    assert_eq!(method, "thread/queue/delete");
    assert_eq!(params["queuedSubmissionId"], "q");
    assert_eq!(params["threadId"], "t");
}
