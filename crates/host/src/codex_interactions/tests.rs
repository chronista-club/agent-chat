use super::*;

fn question() -> Value {
    json!({"threadId":"thread","turnId":"turn","isBlocking":false,"questions":[
        {"id":"one","question":"同じ文面","header":"一つ目","isSecret":true},
        {"id":"two","question":"同じ文面","header":"二つ目","isOther":true,"options":[{"label":"A","description":"候補"}]}
    ]})
}

fn allow(answers: Value) -> Decision {
    Decision::Allow {
        answers: Some(answers),
    }
}

const ALLOW: Decision = Decision::Allow { answers: None };

fn tool_call(server: &str, persist: Value) -> Value {
    json!({"threadId":"thread","turnId":"turn","serverName":server,
        "mode":"form","message":"Allow GetSession?",
        "requestedSchema":{"type":"object","properties":{}},
        "_meta":{"codex_approval_kind":"mcp_tool_call","persist":persist,
            "tool_title":"GetSession","tool_params":{"session_id":"s"}}})
}

// ── VP 由来（mem_1CeySwxuoVc17bGLnU5Np3）──────────────────────────────

#[test]
fn mcp_turn_is_correlation_not_authority() {
    let mut pending = Interactions::default();
    let mut params = json!({"threadId":"thread","turnId":"finished-turn","serverName":"test","mode":"url","url":"https://example.com/"});
    pending
        .receive(
            &json!(1),
            "mcpServer/elicitation/request",
            &params,
            Some("thread"),
            Some("new-turn"),
        )
        .expect("同じ会話の MCP 要求は古い turn の相関情報でも受ける");
    assert!(!pending.observe("turn/completed", &json!({"turn":{"id":"finished-turn"}})));
    assert!(
        pending
            .receive(
                &json!(2),
                "mcpServer/elicitation/request",
                &params,
                Some("other"),
                None
            )
            .is_err()
    );
    params["turnId"] = json!(123);
    assert!(
        pending
            .receive(
                &json!(2),
                "mcpServer/elicitation/request",
                &params,
                Some("thread"),
                None
            )
            .is_err()
    );
    pending.clear();
    assert!(pending.snapshot().is_empty());
}

#[test]
fn mcp_form_preserves_types_and_validates_before_reply() {
    let mut pending = Interactions::default();
    let params = json!({"threadId":"thread","turnId":null,"serverName":"documents","mode":"form","message":"設定してください",
        "requestedSchema":{"type":"object","properties":{
            "count":{"type":"integer","minimum":1,"maximum":5},
            "enabled":{"type":"boolean"}, "note":{"type":"string"}},"required":["count","enabled"]}});
    let id = pending
        .receive(
            &json!("mcp-1"),
            "mcpServer/elicitation/request",
            &params,
            Some("thread"),
            None,
        )
        .expect("turn がない MCP form を表示する");
    assert_eq!(
        pending.get(&id).unwrap().kind,
        InteractionKind::McpElicitation
    );
    assert!(!pending.observe("turn/completed", &json!({"turn":{"id":"turn"}})));
    for answers in [
        json!({"field:count":"9","field:enabled":"false"}),
        json!({"field:count":"2"}),
        json!({"field:count":"2","field:enabled":"false","injected":"yes"}),
    ] {
        assert!(pending.begin_response(&id, &allow(answers)).is_err());
    }
    assert_eq!(
        pending
            .begin_response(
                &id,
                &allow(json!({"field:count":"2","field:enabled":"false"}))
            )
            .unwrap(),
        json!({"id":"mcp-1","result":{"action":"accept","content":{"count":2,"enabled":false},"_meta":null}})
    );
    assert!(pending.observe("serverRequest/resolved", &json!({"requestId":"mcp-1"})));
}

#[test]
fn mcp_url_decline_cancel_and_unsupported_schema() {
    for (mode, url, decision, action) in [
        (
            "url",
            "https://example.com/verify",
            allow(json!({"action":"accept"})),
            "accept",
        ),
        (
            "url",
            "https://example.com/verify",
            Decision::Cancel,
            "cancel",
        ),
        ("url", "javascript:alert(1)", Decision::Deny, "decline"),
        ("openai/userVerification", "", Decision::Deny, "decline"),
    ] {
        let mut pending = Interactions::default();
        let params = json!({"threadId":"thread","turnId":null,"serverName":"test","mode":mode,"url":url,"message":"手続き"});
        assert!(
            pending
                .receive(
                    &json!(1),
                    "mcpServer/elicitation/request",
                    &params,
                    Some("other"),
                    None
                )
                .is_err()
        );
        let id = pending
            .receive(
                &json!(1),
                "mcpServer/elicitation/request",
                &params,
                Some("thread"),
                None,
            )
            .unwrap();
        if !url.starts_with("https://") {
            let view = pending.get(&id).unwrap();
            assert!(!view.can_accept);
            assert!(matches!(view.notice, Some(Notice::UnsupportedMcp(_))));
        }
        assert_eq!(
            pending.begin_response(&id, &decision).unwrap()["result"],
            json!({"action":action,"content":null,"_meta":null})
        );
    }
}

#[test]
fn permissions_preserve_constraints_and_reject_unknown_formats() {
    let files = json!({"read":null,"write":null,"globScanMaxDepth":3,"entries":[
        {"access":"write","path":{"type":"path","path":"/data"}},
        {"access":"deny","path":{"type":"glob_pattern","pattern":"/data/secrets/**"}}
    ]});
    let mut pending = Interactions::default();
    let mut params =
        json!({"threadId":"thread","turnId":"turn","permissions":{"fileSystem":files}});
    let id = pending
        .receive(
            &json!(1),
            "item/permissions/requestApproval",
            &params,
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert!(
        pending.get(&id).unwrap().questions[0]
            .question
            .contains("アクセス禁止")
    );
    assert_eq!(
        pending
            .begin_response(&id, &allow(json!({"p0":"allow"})))
            .unwrap()["result"],
        json!({"permissions":{"fileSystem":files},"scope":"turn"})
    );
    pending.clear();
    params["permissions"]["fileSystem"]["entries"][0]["path"]["futureRestriction"] =
        json!("unknown");
    let id = pending
        .receive(
            &json!(2),
            "item/permissions/requestApproval",
            &params,
            Some("thread"),
            Some("turn"),
        )
        .expect("未対応形式も説明して辞退できる");
    let view = pending.get(&id).unwrap();
    assert!(!view.can_accept);
    assert!(matches!(view.notice, Some(Notice::InvalidPermissions(_))));
    // 部分選択でも、全部許可でも、未対応形式は許可しない
    assert!(
        pending
            .begin_response(&id, &allow(json!({"p0":"allow"})))
            .is_err()
    );
    assert!(pending.begin_response(&id, &ALLOW).is_err());
    assert!(pending.observe("turn/completed", &json!({"turn":{"id":"turn"}})));
    assert!(pending.begin_response(&id, &Decision::Deny).is_err());
}

#[test]
fn permissions_select_subset_and_scope() {
    let params = json!({"threadId":"thread","turnId":"turn","itemId":"p",
        "cwd":"/work","environmentId":"local","reason":"資料の参照",
        "permissions":{"network":{"enabled":true},"fileSystem":{"read":["/docs"],"write":["/out"]}}});
    let mut pending = Interactions::default();
    let id = pending
        .receive(
            &json!(42),
            "item/permissions/requestApproval",
            &params,
            Some("thread"),
            Some("turn"),
        )
        .expect("独立権限を表示できる");
    let view = pending.get(&id).unwrap();
    assert_eq!(view.kind, InteractionKind::Permissions);
    assert_eq!(view.detail["environmentId"], "local");
    assert!(
        pending
            .begin_response(&id, &allow(json!({"scope":"forever","p1":"allow"})))
            .is_err()
    );
    assert!(
        pending
            .begin_response(&id, &allow(json!({"scope":"turn","unrequested":"allow"})))
            .is_err()
    );
    assert_eq!(
        pending
            .begin_response(&id, &allow(json!({"scope":"session","p1":"allow"})))
            .unwrap(),
        json!({"id":42,"result":{"permissions":{"fileSystem":{"read":["/docs"],"write":null}},"scope":"session"}})
    );
    pending.clear();
    let id = pending
        .receive(
            &json!(43),
            "item/permissions/requestApproval",
            &params,
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_eq!(
        pending.begin_response(&id, &Decision::Deny).unwrap()["result"],
        json!({"permissions":{},"scope":"turn"})
    );
}

#[test]
fn answers_use_question_ids_and_preserve_native_id_type() {
    let mut pending = Interactions::default();
    let first = pending
        .receive(
            &json!(7),
            "item/tool/requestUserInput",
            &question(),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    let second = pending
        .receive(
            &json!("7"),
            "item/tool/requestUserInput",
            &question(),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    let view = pending.get(&first).unwrap();
    assert!(!view.blocking);
    assert!(view.questions[0].is_secret && !view.questions[0].is_other);
    assert!(view.questions[1].is_other);
    let invalid = allow(json!({"同じ文面":"lost"}));
    assert!(pending.begin_response(&first, &invalid).is_err());
    assert!(pending.begin_response(&first, &ALLOW).is_err());
    assert_eq!(
        pending
            .begin_response(&first, &allow(json!({"one":"secret","two":"A"})))
            .unwrap(),
        json!({"id":7,"result":{"answers":{"one":{"answers":["secret"]},"two":{"answers":["A"]}}}})
    );
    // 送信中は二重回答を受けない。回答本文は台帳に残らない
    assert!(pending.begin_response(&first, &invalid).is_err());
    assert!(!format!("{:?}", pending.snapshot()).contains("\"secret\""));
    assert!(pending.observe("serverRequest/resolved", &json!({"requestId":7})));
    assert_eq!(pending.snapshot().len(), 1);
    assert_eq!(
        pending.begin_response(&second, &Decision::Deny).unwrap(),
        json!({"id":"7","result":{"answers":{}}})
    );
}

#[test]
fn command_accept_cancel_is_answerable() {
    for (decision, expected) in [(ALLOW, "accept"), (Decision::Deny, "cancel")] {
        let mut pending = Interactions::default();
        let id = pending
            .receive(
                &json!("approval"),
                "item/commandExecution/requestApproval",
                &json!({"threadId":"thread","turnId":"turn","command":"printf test",
                "availableDecisions":["accept","acceptForSession","cancel"]}),
                Some("thread"),
                Some("turn"),
            )
            .expect("cancel を提示する承認もカードにする");
        assert!(pending.get(&id).unwrap().cancel_on_deny);
        assert_eq!(
            pending.begin_response(&id, &decision).unwrap(),
            json!({"id":"approval","result":{"decision":expected}})
        );
    }
}

#[test]
fn approval_requires_visible_changes_and_offered_decisions() {
    let mut pending = Interactions::default();
    let params = json!({"threadId":"thread","turnId":"turn","itemId":"file","reason":"修正"});
    let id = pending
        .receive(
            &json!(1),
            "item/fileChange/requestApproval",
            &params,
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_eq!(
        pending.get(&id).unwrap().notice,
        Some(Notice::MissingChanges)
    );
    assert!(pending.begin_response(&id, &ALLOW).is_err());
    pending.clear();
    pending.observe("item/started", &json!({"turnId":"turn","item":{"id":"file","type":"fileChange","changes":[{"path":"a.txt","diff":"+new"}]}}));
    let id = pending
        .receive(
            &json!(2),
            "item/fileChange/requestApproval",
            &params,
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_eq!(
        pending.get(&id).unwrap().detail["changes"][0]["diff"],
        "+new"
    );
    assert_eq!(
        pending.begin_response(&id, &ALLOW).unwrap(),
        json!({"id":2,"result":{"decision":"accept"}})
    );
    let id = pending
        .receive(
            &json!(3),
            "item/commandExecution/requestApproval",
            &json!({"threadId":"thread","turnId":"turn","command":"ls","availableDecisions":["decline"]}),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert!(pending.begin_response(&id, &ALLOW).is_err());
    assert_eq!(
        pending.begin_response(&id, &Decision::Deny).unwrap()["result"],
        json!({"decision":"decline"})
    );
}

#[test]
fn approval_identifies_target_and_releases_file_previews() {
    let mut pending = Interactions::default();
    for (id, params, kind, acceptable) in [
        (
            1,
            json!({"networkApprovalContext":{"host":"example.com","protocol":"https"}}),
            InteractionKind::Network,
            true,
        ),
        (
            2,
            json!({"reason":"必要です","cwd":"/work"}),
            InteractionKind::Command,
            false,
        ),
    ] {
        let mut params = params;
        params["threadId"] = json!("thread");
        params["turnId"] = json!("turn");
        let request = pending
            .receive(
                &json!(id),
                "item/commandExecution/requestApproval",
                &params,
                Some("thread"),
                Some("turn"),
            )
            .unwrap();
        let view = pending.get(&request).unwrap();
        assert_eq!(view.kind, kind);
        assert_eq!(view.can_accept, acceptable);
    }
    pending.clear();
    for id in 0..12 {
        let item = format!("file-{id}");
        pending.observe("item/started", &json!({"turnId":"turn","item":{"id":item,"type":"fileChange","changes":[{"path":"a.txt","diff":"+new"}]}}));
        let request = pending
            .receive(
                &json!(id),
                "item/fileChange/requestApproval",
                &json!({"threadId":"thread","turnId":"turn","itemId":item}),
                Some("thread"),
                Some("turn"),
            )
            .unwrap();
        assert!(
            pending.get(&request).unwrap().can_accept,
            "sequential approval {id}"
        );
        pending.finish_response(&request);
    }
}

#[test]
fn reject_wrong_thread_and_expire_by_turn_and_host_generation() {
    let mut pending = Interactions::default();
    for thread in [Some("other"), Some(""), None] {
        assert!(
            pending
                .receive(
                    &json!(1),
                    "item/tool/requestUserInput",
                    &question(),
                    thread,
                    Some("turn")
                )
                .is_err()
        );
    }
    let id = pending
        .receive(
            &json!(1),
            "item/tool/requestUserInput",
            &question(),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert!(!pending.observe("turn/completed", &json!({"turn":{"id":"other"}})));
    assert!(pending.observe("turn/completed", &json!({"turn":{"id":"turn"}})));
    assert!(pending.begin_response(&id, &ALLOW).is_err());
    let mut fresh = Interactions::default();
    let fresh_id = fresh
        .receive(
            &json!(1),
            "item/tool/requestUserInput",
            &question(),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_ne!(id, fresh_id);
    assert!(fresh.begin_response(&id, &Decision::Deny).is_err());
}

// ── GFP 由来（design 08、2026-09-29 の会話内許可とフルアクセス）───────────

/// Codex のツール実行承認は空フォーム。表示は MCP 承認として、応答は action だけ返す
/// （実 Codex 0.158.0 で確認した形）。
#[test]
fn tool_call_approval_is_its_own_kind_with_plain_actions() {
    let mut pending = Interactions::default();
    let params = tool_call("gfp-estimate", json!(["session", "always"]));
    let id = pending
        .receive(
            &json!(42),
            "mcpServer/elicitation/request",
            &params,
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    let view = pending.get(&id).unwrap();
    assert_eq!(view.kind, InteractionKind::McpApproval);
    assert_eq!(view.server.as_deref(), Some("gfp-estimate"));
    assert_eq!(view.detail["tool_params"], params["_meta"]["tool_params"]);
    assert_eq!(view.detail["message"], "Allow GetSession?");
    assert_eq!(view.elicitation.as_ref().unwrap().fields.len(), 0);
    for (decision, action) in [
        (ALLOW, "accept"),
        (allow(json!({})), "accept"),
        (allow(json!({"action":"accept"})), "accept"),
        (Decision::Deny, "decline"),
        (Decision::Cancel, "cancel"),
    ] {
        assert_eq!(
            pending.response(&id, &decision).unwrap(),
            json!({"id":42,"result":{"action":action}})
        );
    }
    // 空フォームに値は載せない
    assert!(
        pending
            .response(&id, &allow(json!({"field:x":"1"})))
            .is_err()
    );
    assert!(pending.needs_interrupt(&id));
    // ツール承認は turn に属する。一般の MCP 手続きと違い turn の終了で片付ける
    assert!(pending.observe("turn/completed", &json!({"turn":{"id":"turn"}})));
}

/// Codex が「この会話中は記憶」を示した要求だけ会話内許可を返す。always は使わない。
#[test]
fn tool_call_approval_can_be_remembered_only_when_offered() {
    let mut pending = Interactions::default();
    let id = pending
        .receive(
            &json!(42),
            "mcpServer/elicitation/request",
            &tool_call("gfp-estimate", json!(["session", "always"])),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert!(pending.get(&id).unwrap().can_remember);
    assert_eq!(
        pending.response(&id, &Decision::AllowSession).unwrap(),
        json!({"id":42,"result":{"action":"accept","_meta":{"persist":"session"}}})
    );

    let mut always = Interactions::default();
    let id = always
        .receive(
            &json!(7),
            "mcpServer/elicitation/request",
            &tool_call("gfp-estimate", json!(["always"])),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert!(!always.get(&id).unwrap().can_remember);
    assert!(always.response(&id, &Decision::AllowSession).is_err());

    // ツール承認以外は会話内許可にできない
    let mut command = Interactions::default();
    let id = command
        .receive(
            &json!(1),
            "item/commandExecution/requestApproval",
            &json!({"threadId":"thread","turnId":"turn","command":"ls"}),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert!(command.response(&id, &Decision::AllowSession).is_err());
}

/// 自動許可は信頼するサーバーのツール承認だけ。一般フォーム・他サーバー・コマンドは人に聞く。
#[test]
fn auto_accept_is_limited_to_trusted_tool_calls() {
    let trusted = ["gfp-estimate"];
    let mut pending = Interactions::default();
    let id = pending
        .receive(
            &json!(1),
            "mcpServer/elicitation/request",
            &tool_call("gfp-estimate", json!(["session"])),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_eq!(
        pending.auto_accept(&id, &trusted),
        Some(json!({"id":1,"result":{"action":"accept"}}))
    );
    assert_eq!(pending.auto_accept(&id, &[]), None);

    let other = pending
        .receive(
            &json!(2),
            "mcpServer/elicitation/request",
            &tool_call("personal", json!(["session"])),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_eq!(pending.auto_accept(&other, &trusted), None);

    let mut form = tool_call("gfp-estimate", json!(["session"]));
    form["requestedSchema"]["properties"] = json!({"password":{"type":"string"}});
    let form = pending
        .receive(
            &json!(3),
            "mcpServer/elicitation/request",
            &form,
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_eq!(
        pending.get(&form).unwrap().kind,
        InteractionKind::McpElicitation
    );
    assert_eq!(pending.auto_accept(&form, &trusted), None);

    let command = pending
        .receive(
            &json!(4),
            "item/commandExecution/requestApproval",
            &json!({"threadId":"thread","turnId":"turn","command":"ls"}),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_eq!(pending.auto_accept(&command, &trusted), None);
    assert!(!pending.needs_interrupt(&command));
}

/// 部分選択の無い許可は、要求された権限をそのまま turn 限定で返す。
#[test]
fn permissions_allow_without_selection_grants_exactly_the_request_for_one_turn() {
    let requested = json!({"network":{"enabled":true},"fileSystem":{"read":["/資料"]}});
    let mut pending = Interactions::default();
    let id = pending
        .receive(
            &json!(1),
            "item/permissions/requestApproval",
            &json!({"threadId":"thread","turnId":"turn","permissions":requested}),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert_eq!(pending.get(&id).unwrap().detail["permissions"], requested);
    assert_eq!(
        pending.response(&id, &ALLOW).unwrap()["result"],
        json!({"permissions":requested,"scope":"turn"})
    );
    assert_eq!(
        pending.response(&id, &Decision::Cancel).unwrap()["result"],
        json!({"permissions":{},"scope":"turn"})
    );
    assert!(pending.needs_interrupt(&id));
}

/// response は検証だけで消費しない。begin_response は送信中の二重回答を止める。
#[test]
fn response_is_pure_and_begin_response_guards_double_reply() {
    let mut pending = Interactions::default();
    let id = pending
        .receive(
            &json!(1),
            "item/commandExecution/requestApproval",
            &json!({"threadId":"thread","turnId":"turn","command":"ls"}),
            Some("thread"),
            Some("turn"),
        )
        .unwrap();
    assert!(pending.response(&id, &ALLOW).is_ok());
    assert!(pending.response(&id, &Decision::Deny).is_ok());
    assert_eq!(
        pending.response(&id, &Decision::Cancel).unwrap()["result"],
        json!({"decision":"cancel"})
    );
    assert!(pending.begin_response(&id, &ALLOW).is_ok());
    assert!(pending.begin_response(&id, &Decision::Deny).is_err());
    pending.finish_response(&id);
    assert!(pending.response(&id, &ALLOW).is_err());
}
