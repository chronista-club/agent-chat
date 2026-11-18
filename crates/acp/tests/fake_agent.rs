//! 偽の agent（メモリの中の JSON-RPC の相手）で host を確かめる。
//!
//! 実 CLI 無しで、load の流れ・turn・中断・設定の切替・知らない通知・EOF・立て直しを決まった形で再現する。
//! 相手は SDK の `Channel::duplex()` の片側で、生の JSON-RPC を台本どおりに返す。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_chat_acp::{
    AcpClientHost, Connector, HistoryLimits, HostError, HostOptions, LoadFallback,
    PermissionDecision, PromptImage,
};
use agent_chat_protocol::ConversationEvent as E;
use agent_client_protocol::{Channel, RawJsonRpcMessage, RawJsonRpcParams, TransportFrame};
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::broadcast;

const WAIT: Duration = Duration::from_secs(5);

/// 1 回の `session/prompt` への台本。
#[derive(Clone, Default)]
struct Turn {
    updates: Vec<Value>,
    /// `session/cancel` が来るまで応答しない。
    wait_cancel: bool,
    /// 応答の前に permission を求める（options）。
    permission: Option<Value>,
    /// prompt の応答を error にする。
    error: bool,
    /// updates を送った後、応答せずに接続を閉じる。
    eof: bool,
}

/// 接続 1 本ぶんの台本。
#[derive(Clone, Default)]
struct Script {
    /// initialize に答えない。
    hang: bool,
    /// load の応答の前に流す update。
    replay: Vec<Value>,
    /// load の応答の後に流す update。
    after_load: Vec<Value>,
    load_error: bool,
    /// `session/set_config_option` に答えない。
    config_hang: bool,
    config: Vec<Value>,
    turns: VecDeque<Turn>,
}

type Log = Arc<Mutex<Vec<(String, Value)>>>;

fn params(params: Option<&RawJsonRpcParams>) -> Value {
    params
        .cloned()
        .map_or(Value::Null, RawJsonRpcParams::into_value)
}

async fn fake_agent(mut peer: Channel, mut script: Script, log: Log) {
    let send = |peer: &Channel, message: RawJsonRpcMessage| {
        let _ = peer.tx.unbounded_send(TransportFrame::Single(message));
    };
    let update = |peer: &Channel, session: &str, update: &Value| {
        let message = RawJsonRpcMessage::notification(
            "session/update".to_owned(),
            json!({"sessionId": session, "update": update}),
        )
        .expect("update");
        let _ = peer.tx.unbounded_send(TransportFrame::Single(message));
    };
    let mut session = "new-1".to_string();
    let mut pending_prompt = None;
    let mut pending_permission = None;
    while let Some(frame) = peer.rx.next().await {
        let TransportFrame::Single(message) = frame else {
            continue;
        };
        match message {
            RawJsonRpcMessage::Request(request) => {
                let p = params(request.params.as_ref());
                log.lock()
                    .unwrap()
                    .push((request.method.to_string(), p.clone()));
                let result = match &*request.method {
                    "initialize" => {
                        if script.hang {
                            continue;
                        }
                        Ok(
                            json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}),
                        )
                    }
                    "session/new" => {
                        session = "new-1".to_string();
                        Ok(json!({"sessionId": session, "configOptions": script.config}))
                    }
                    "session/load" => {
                        session = p["sessionId"].as_str().unwrap_or_default().to_string();
                        if script.load_error {
                            Err(agent_client_protocol::Error::invalid_params())
                        } else {
                            for u in &script.replay {
                                update(&peer, &session, u);
                            }
                            send(
                                &peer,
                                RawJsonRpcMessage::response(
                                    request.id.clone(),
                                    Ok(json!({"configOptions": script.config})),
                                ),
                            );
                            for u in &script.after_load {
                                update(&peer, &session, u);
                            }
                            continue;
                        }
                    }
                    "session/set_config_option" if script.config_hang => continue,
                    "session/set_config_option" => {
                        let (id, value) = (p["configId"].clone(), p["value"].clone());
                        if value == "bad" {
                            Err(agent_client_protocol::Error::invalid_params())
                        } else {
                            for option in &mut script.config {
                                if option["id"] == id {
                                    option["currentValue"] = value.clone();
                                }
                            }
                            Ok(json!({"configOptions": script.config}))
                        }
                    }
                    "session/prompt" => {
                        let turn = script.turns.pop_front().unwrap_or_default();
                        for u in &turn.updates {
                            update(&peer, &session, u);
                        }
                        if turn.eof {
                            return;
                        }
                        if let Some(options) = turn.permission {
                            send(
                                &peer,
                                RawJsonRpcMessage::request(
                                    "session/request_permission".to_owned(),
                                    json!({"sessionId": session, "toolCall": {"toolCallId": "t1"}, "options": options}),
                                    "perm-1".to_string(),
                                )
                                .expect("permission"),
                            );
                            pending_permission = Some(request.id.clone());
                            continue;
                        }
                        if turn.wait_cancel {
                            pending_prompt = Some(request.id.clone());
                            continue;
                        }
                        if turn.error {
                            Err(agent_client_protocol::Error::internal_error())
                        } else {
                            Ok(json!({"stopReason": "end_turn"}))
                        }
                    }
                    _ => Err(agent_client_protocol::Error::method_not_found()),
                };
                send(&peer, RawJsonRpcMessage::response(request.id, result));
            }
            RawJsonRpcMessage::Notification(notification) => {
                log.lock().unwrap().push((
                    notification.method.to_string(),
                    params(notification.params.as_ref()),
                ));
                if &*notification.method == "session/cancel"
                    && let Some(id) = pending_prompt.take()
                {
                    send(
                        &peer,
                        RawJsonRpcMessage::response(id, Ok(json!({"stopReason": "cancelled"}))),
                    );
                }
            }
            RawJsonRpcMessage::Response(response) => {
                let value = serde_json::to_value(&response).unwrap_or_default();
                log.lock()
                    .unwrap()
                    .push(("permission_response".to_string(), value));
                if let Some(id) = pending_permission.take() {
                    send(
                        &peer,
                        RawJsonRpcMessage::response(id, Ok(json!({"stopReason": "end_turn"}))),
                    );
                }
            }
        }
    }
}

struct FakeConnector {
    scripts: VecDeque<Script>,
    log: Log,
}

impl Connector for FakeConnector {
    type Transport = Channel;
    fn connect(&mut self) -> Result<Channel, String> {
        let script = self.scripts.pop_front().ok_or("台本がもう無い")?;
        let (client, agent) = Channel::duplex();
        tokio::spawn(fake_agent(agent, script, Arc::clone(&self.log)));
        Ok(client)
    }
}

struct Fixture {
    host: AcpClientHost,
    events: broadcast::Receiver<E>,
    log: Log,
}

fn start(scripts: Vec<Script>, options: HostOptions) -> Fixture {
    let log = Log::default();
    let connector = FakeConnector {
        scripts: scripts.into(),
        log: Arc::clone(&log),
    };
    let host = AcpClientHost::spawn_with(connector, PathBuf::from("/work"), options).expect("起動");
    let events = host.subscribe();
    Fixture { host, events, log }
}

impl Fixture {
    /// `stop` を満たす event が来るまで集める（その event を含む）。
    async fn until(&mut self, stop: impl Fn(&E) -> bool) -> Vec<E> {
        let mut out = Vec::new();
        loop {
            let event = tokio::time::timeout(WAIT, self.events.recv())
                .await
                .unwrap_or_else(|_| panic!("event を待ち切れない。ここまで: {out:?}"))
                .expect("broadcast");
            let done = stop(&event);
            out.push(event);
            if done {
                return out;
            }
        }
    }

    async fn until_turn_end(&mut self) -> Vec<E> {
        self.until(|e| matches!(e, E::TurnCompleted { .. })).await
    }

    fn methods(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }

    fn params_of(&self, method: &str) -> Vec<Value> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

fn agent_text(text: &str) -> Value {
    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
}
fn user_text(text: &str) -> Value {
    json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": text}})
}
fn thought_text(text: &str) -> Value {
    json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": text}})
}
fn user(text: &str) -> E {
    E::UserMessage { text: text.into() }
}
fn msg(text: &str) -> E {
    E::MessageChunk { text: text.into() }
}
fn resume(session: &str) -> HostOptions {
    HostOptions {
        resume: Some(session.to_string()),
        ..HostOptions::default()
    }
}
fn config() -> Vec<Value> {
    vec![
        json!({"id": "model", "name": "Model", "type": "select", "currentValue": "a",
               "options": [{"value": "a", "name": "A"}, {"value": "b", "name": "B"}]}),
        json!({"id": "thought_level", "name": "Thinking", "type": "select", "currentValue": "low",
               "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]}),
    ]
}

#[tokio::test]
async fn new_session_turn_streams_events_and_records_history() {
    let turn = Turn {
        updates: vec![
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "status": "in_progress", "rawInput": {"cmd": "ls"}}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                   "content": [{"type": "content", "content": {"type": "text", "text": "a.txt"}}]}),
            agent_text("do"),
            agent_text("ne"),
            json!({"sessionUpdate": "usage_update", "used": 120, "size": 1000}),
        ],
        ..Turn::default()
    };
    let mut f = start(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("hi", &[]).unwrap();
    let events = f.until_turn_end().await;
    let tool_call = E::ToolCall {
        id: "t1".into(),
        name: "ls".into(),
        input: json!({"cmd": "ls"}),
    };
    let tool_result = E::ToolCallUpdate {
        tool_use_id: "t1".into(),
        content: "a.txt".into(),
        is_error: false,
    };
    assert!(
        matches!(&events[0], E::SessionInit { session_id, cwd, .. } if session_id == "new-1" && cwd.as_deref() == Some("/work")),
        "{events:?}"
    );
    assert_eq!(
        events[1..],
        [
            tool_call.clone(),
            tool_result.clone(),
            msg("do"),
            msg("ne"),
            E::TurnCompleted {
                session_id: "new-1".into(),
                cost_usd: None,
                context_tokens: Some(120),
                context_window: Some(1000),
            },
        ]
    );
    let history = f.host.history().await.unwrap();
    assert_eq!(
        history.events,
        [user("hi"), tool_call, tool_result, msg("done")]
    );
    assert!(!history.in_flight && !history.truncated);
    assert_eq!(f.methods(), ["initialize", "session/new", "session/prompt"]);
    assert_eq!(f.params_of("session/new")[0]["cwd"], "/work");
    assert_eq!(
        f.params_of("session/prompt")[0]["prompt"],
        json!([{"type": "text", "text": "hi"}])
    );
}

/// load が流す過去の会話は、broadcast に流さず履歴として渡す。
#[tokio::test]
async fn load_stream_becomes_history_not_live_events() {
    let script = Script {
        replay: vec![
            user_text("最初の質問"),
            thought_text("考え"),
            json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "ls", "status": "completed", "rawInput": {},
                   "content": [{"type": "content", "content": {"type": "text", "text": "out"}}]}),
            agent_text("答え"),
            user_text("次の質問"),
            agent_text("次の答え"),
        ],
        after_load: vec![
            json!({"sessionUpdate": "usage_update", "used": 5, "size": 10}),
            json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "compact", "description": "縮める"}]}),
            json!({"sessionUpdate": "session_info_update", "title": "t"}),
        ],
        turns: [Turn {
            updates: vec![agent_text("続き")],
            ..Turn::default()
        }]
        .into(),
        ..Script::default()
    };
    let mut f = start(vec![script], resume("s-1"));
    let history = f.host.history().await.unwrap();
    assert_eq!(
        history.events,
        [
            user("最初の質問"),
            E::ThoughtChunk {
                text: "考え".into()
            },
            E::ToolCall {
                id: "c1".into(),
                name: "ls".into(),
                input: json!({})
            },
            E::ToolCallUpdate {
                tool_use_id: "c1".into(),
                content: "out".into(),
                is_error: false
            },
            msg("答え"),
            user("次の質問"),
            msg("次の答え"),
        ]
    );
    assert!(!history.in_flight);
    let load = &f.params_of("session/load")[0];
    assert_eq!(
        (load["sessionId"].as_str(), load["cwd"].as_str()),
        (Some("s-1"), Some("/work"))
    );

    // 会話中の event だけが broadcast に出る。過去の会話は出ない
    f.host.submit("続けて", &[]).unwrap();
    let events = f.until_turn_end().await;
    let live: Vec<_> = events
        .iter()
        .filter(|e| !matches!(e, E::SessionInit { .. }))
        .cloned()
        .collect();
    assert_eq!(live.len(), 2, "{events:?}");
    assert_eq!(live[0], msg("続き"));
    assert!(
        matches!(
            &live[1],
            E::TurnCompleted {
                context_tokens: Some(5),
                context_window: Some(10),
                ..
            }
        ),
        "load の後の usage を載せる: {live:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, E::SessionInit { slash_commands, .. } if slash_commands == &["compact"])
        ),
        "load の後の slash command を SessionInit に載せる: {events:?}"
    );
    let history = f.host.history().await.unwrap();
    assert_eq!(history.events[7..], [user("続けて"), msg("続き")]);
}

/// engine が生きたまま開き直す時は控えから出す。turn の最中でも出せる。
#[tokio::test]
async fn history_is_available_mid_turn_and_interrupt_ends_the_turn() {
    let turn = Turn {
        updates: vec![agent_text("途中")],
        wait_cancel: true,
        ..Turn::default()
    };
    let mut f = start(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("長い仕事", &[]).unwrap();
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    let history = f.host.history().await.unwrap();
    assert_eq!(history.events, [user("長い仕事"), msg("途中")]);
    assert!(history.in_flight);
    assert_eq!(
        f.host.restart().await,
        Err(HostError::Busy),
        "turn の最中は立て直さない"
    );

    f.host.interrupt().unwrap();
    f.until_turn_end().await;
    assert_eq!(f.params_of("session/cancel")[0]["sessionId"], "new-1");
    assert!(!f.host.history().await.unwrap().in_flight);
}

/// engine が送った発話を返してきても（agy 1.5.9）、二重にしない。
#[tokio::test]
async fn own_prompt_echo_is_neither_broadcast_nor_duplicated() {
    let turn = Turn {
        updates: vec![user_text("hi"), agent_text("yo")],
        ..Turn::default()
    };
    let mut f = start(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("hi", &[]).unwrap();
    let events = f.until_turn_end().await;
    assert!(
        !events.iter().any(|e| matches!(e, E::UserMessage { .. })),
        "{events:?}"
    );
    assert_eq!(
        f.host.history().await.unwrap().events,
        [user("hi"), msg("yo")]
    );
}

/// 他の書き手が進めたら process を立て直す。控えは engine の保存から取り直す。
#[tokio::test]
async fn restart_reconnects_loads_and_replaces_history() {
    let first = Script {
        turns: [Turn {
            updates: vec![agent_text("一")],
            ..Turn::default()
        }]
        .into(),
        ..Script::default()
    };
    let second = Script {
        replay: vec![
            user_text("hi"),
            agent_text("一"),
            user_text("console から"),
            agent_text("二"),
        ],
        turns: [Turn {
            updates: vec![agent_text("三")],
            ..Turn::default()
        }]
        .into(),
        ..Script::default()
    };
    let mut f = start(vec![first, second], HostOptions::default());
    f.host.submit("hi", &[]).unwrap();
    f.until_turn_end().await;

    let history = f.host.restart().await.unwrap();
    assert_eq!(
        history.events,
        [user("hi"), msg("一"), user("console から"), msg("二")]
    );
    assert_eq!(f.host.history().await.unwrap(), history);
    assert_eq!(
        f.methods(),
        [
            "initialize",
            "session/new",
            "session/prompt",
            "initialize",
            "session/load"
        ]
    );
    assert_eq!(f.params_of("session/load")[0]["sessionId"], "new-1");

    // 同じ subscribe のまま続けられる
    f.host.submit("続き", &[]).unwrap();
    let events = f.until_turn_end().await;
    assert!(events.contains(&msg("三")), "{events:?}");
}

#[tokio::test]
async fn load_failure_fails_by_default() {
    let mut f = start(
        vec![Script {
            load_error: true,
            ..Script::default()
        }],
        resume("gone"),
    );
    let error = f.host.history().await.unwrap_err();
    assert!(
        matches!(&error, HostError::Establish(m) if m.contains("session/load")),
        "{error:?}"
    );
    let events = f.until(|e| matches!(e, E::EngineExited { .. })).await;
    assert!(!events.iter().any(|e| matches!(e, E::SessionInit { .. })));
    assert_eq!(
        f.methods(),
        ["initialize", "session/load"],
        "黙って新規に変えない"
    );
    assert_eq!(f.host.submit("x", &[]), Err(error));
}

#[tokio::test]
async fn load_failure_starts_new_session_when_policy_says_so() {
    let mut f = start(
        vec![Script {
            load_error: true,
            ..Script::default()
        }],
        HostOptions {
            on_load_failed: LoadFallback::NewSession,
            ..resume("gone")
        },
    );
    let events = f.until(|e| matches!(e, E::SessionInit { .. })).await;
    assert!(
        matches!(&events[0], E::Notice { .. }),
        "新規に変えた事実を出す: {events:?}"
    );
    assert!(matches!(&events[1], E::SessionInit { session_id, .. } if session_id == "new-1"));
    assert!(f.host.history().await.unwrap().events.is_empty());
}

#[tokio::test]
async fn establish_timeout_is_reported() {
    let f = start(
        vec![Script {
            hang: true,
            ..Script::default()
        }],
        HostOptions {
            establish_timeout: Duration::from_millis(50),
            ..HostOptions::default()
        },
    );
    let error = tokio::time::timeout(WAIT, f.host.history())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, HostError::Establish(_)), "{error:?}");
}

/// 設定: load の後に意図を当て直してから prompt を送る。切替の成功と失敗。
#[tokio::test]
async fn config_intent_is_reapplied_and_set_config_reports_results() {
    let script = Script {
        config: config(),
        turns: [Turn::default()].into(),
        ..Script::default()
    };
    let mut f = start(
        vec![script],
        HostOptions {
            config_intent: vec![
                ("model".into(), "b".into()),
                ("thought_level".into(), "low".into()),
                ("gone_axis".into(), "x".into()),
            ],
            ..resume("s-1")
        },
    );
    f.host.submit("hi", &[]).unwrap();
    let events = f.until_turn_end().await;
    assert_eq!(
        f.methods(),
        [
            "initialize",
            "session/load",
            "session/set_config_option",
            "session/prompt"
        ],
        "現在値と同じ軸・engine に無い軸は当てない"
    );
    assert!(
        matches!(&events[0], E::SessionInit { model: Some(m), .. } if m == "b"),
        "{events:?}"
    );
    let view = f.host.config_view().unwrap();
    assert_eq!(view.options[0].current_value, "b");

    let options = f.host.set_config("thought_level", "high").await.unwrap();
    assert_eq!(options[1].current_value, "high");
    let error = f.host.set_config("model", "bad").await.unwrap_err();
    assert!(matches!(error, HostError::Engine(_)), "{error:?}");
    let view = f.host.config_view().unwrap();
    assert!(view.error.is_some());
    assert_eq!(
        view.options[1].current_value, "high",
        "失敗しても設定面は残る"
    );
}

/// 知らない種類の update は読み飛ばして続ける。
#[tokio::test]
async fn unknown_updates_are_skipped() {
    let turn = Turn {
        updates: vec![
            json!({"sessionUpdate": "hook_execution", "event_name": "stop", "runs": []}),
            json!({"no_session_update_key": true}),
            agent_text("ok"),
        ],
        ..Turn::default()
    };
    let mut f = start(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("hi", &[]).unwrap();
    let events = f.until_turn_end().await;
    assert!(events.contains(&msg("ok")), "{events:?}");
}

#[tokio::test]
async fn engine_eof_is_reported_and_later_calls_fail() {
    let turn = Turn {
        updates: vec![agent_text("途中")],
        eof: true,
        ..Turn::default()
    };
    let mut f = start(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("hi", &[]).unwrap();
    let events = f.until(|e| matches!(e, E::EngineExited { .. })).await;
    assert!(
        events.contains(&msg("途中")),
        "EOF の前に届いた分は落とさない: {events:?}"
    );
    tokio::time::timeout(WAIT, async {
        while f.host.submit("again", &[]).is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("終了の後の submit は Err");
    assert!(matches!(f.host.history().await, Err(HostError::Dead(_))));
}

#[tokio::test]
async fn prompt_error_is_reported_and_turn_still_ends() {
    let mut f = start(
        vec![Script {
            turns: [
                Turn {
                    error: true,
                    ..Turn::default()
                },
                Turn {
                    updates: vec![agent_text("次")],
                    ..Turn::default()
                },
            ]
            .into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("a", &[]).unwrap();
    let events = f.until_turn_end().await;
    assert!(
        matches!(events[events.len() - 2], E::Error { .. }),
        "{events:?}"
    );
    f.host.submit("b", &[]).unwrap();
    assert!(
        f.until_turn_end().await.contains(&msg("次")),
        "エラーの後も続けられる"
    );
}

#[tokio::test]
async fn permission_request_is_answered_by_policy() {
    let options = json!([
        {"optionId": "no", "name": "No", "kind": "reject_once"},
        {"optionId": "once", "name": "Yes", "kind": "allow_once"},
    ]);
    let script = |permission: &Value| Script {
        turns: [Turn {
            permission: Some(permission.clone()),
            ..Turn::default()
        }]
        .into(),
        ..Script::default()
    };
    let mut f = start(vec![script(&options)], HostOptions::default());
    f.host.submit("hi", &[]).unwrap();
    f.until_turn_end().await;
    let answer = &f.params_of("permission_response")[0];
    assert_eq!(
        answer["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "once"}),
        "{answer}"
    );

    let mut f = start(
        vec![script(&options)],
        HostOptions {
            permission: Arc::new(|_| PermissionDecision::Cancelled),
            ..HostOptions::default()
        },
    );
    f.host.submit("hi", &[]).unwrap();
    f.until_turn_end().await;
    let answer = &f.params_of("permission_response")[0];
    assert_eq!(
        answer["result"]["outcome"]["outcome"], "cancelled",
        "{answer}"
    );
}

/// turn の最中に送った発話は、終わってから順に送る。画像は image block で並べる。
#[tokio::test]
async fn queued_prompts_go_one_at_a_time_with_images() {
    let turns = [
        Turn {
            updates: vec![agent_text("1")],
            wait_cancel: true,
            ..Turn::default()
        },
        Turn {
            updates: vec![agent_text("2")],
            ..Turn::default()
        },
    ];
    let mut f = start(
        vec![Script {
            turns: turns.into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("first", &[]).unwrap();
    let image = PromptImage {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    };
    f.host.submit("second", &[image]).unwrap();
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    assert_eq!(
        f.params_of("session/prompt").len(),
        1,
        "1 つ目の turn の最中は送らない"
    );
    f.host.interrupt().unwrap();
    f.until_turn_end().await;
    f.until_turn_end().await;
    let prompts = f.params_of("session/prompt");
    assert_eq!(
        prompts[1]["prompt"],
        json!([{"type": "text", "text": "second"}, {"type": "image", "data": "AAAA", "mimeType": "image/png"}])
    );
    assert_eq!(
        f.host.history().await.unwrap().events,
        [user("first"), msg("1"), user("second"), msg("2")]
    );
}

/// turn の最中に slash command が変わったら、`SessionInit` は turn の終わり（`TurnCompleted` の直前）に出し直す。
#[tokio::test]
async fn session_init_refresh_waits_for_turn_end() {
    let turn = Turn {
        updates: vec![
            json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "goal"}]}),
            agent_text("x"),
        ],
        ..Turn::default()
    };
    let mut f = start(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("hi", &[]).unwrap();
    let events = f.until_turn_end().await;
    let n = events.len();
    assert_eq!(
        events[n - 3],
        msg("x"),
        "turn の途中では出し直さない: {events:?}"
    );
    assert!(
        matches!(&events[n - 2], E::SessionInit { slash_commands, .. } if slash_commands == &["goal"]),
        "{events:?}"
    );
    assert_eq!(f.host.session_init().as_ref(), Some(&events[n - 2]));
}

#[tokio::test]
async fn session_callback_gets_id_and_its_failure_is_reported() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut f = start(
        vec![Script::default()],
        HostOptions {
            on_session: Arc::new(move |id| {
                sink.lock().unwrap().push(id.to_string());
                Err("disk full".to_string())
            }),
            ..HostOptions::default()
        },
    );
    let events = f.until(|e| matches!(e, E::SessionInit { .. })).await;
    assert_eq!(*seen.lock().unwrap(), ["new-1"]);
    assert!(
        matches!(&events[0], E::Error { message } if message.contains("disk full")),
        "{events:?}"
    );
}

/// load の流れにも同じ上限を掛ける。
#[tokio::test]
async fn history_limits_apply_to_the_load_stream() {
    let script = Script {
        replay: vec![
            user_text("1"),
            agent_text("a"),
            user_text("2"),
            agent_text("b"),
            user_text("3"),
            agent_text("c"),
        ],
        ..Script::default()
    };
    let f = start(
        vec![script],
        HostOptions {
            history_limits: HistoryLimits {
                max_events: 4,
                ..HistoryLimits::default()
            },
            ..resume("s-1")
        },
    );
    let history = f.host.history().await.unwrap();
    assert_eq!(history.events, [user("2"), msg("b"), user("3"), msg("c")]);
    assert!(history.truncated);
}

/// 会話中に切り替えた設定は、立て直しの後も保つ（起動の時の意図に戻さない）。
#[tokio::test]
async fn config_chosen_mid_conversation_survives_restart() {
    let script = || Script {
        config: config(),
        ..Script::default()
    };
    let mut f = start(
        vec![script(), script()],
        HostOptions {
            config_intent: vec![("model".into(), "a".into())],
            ..HostOptions::default()
        },
    );
    f.until(|e| matches!(e, E::SessionInit { .. })).await;
    f.host.set_config("model", "b").await.unwrap();
    f.host.set_config("thought_level", "high").await.unwrap();

    // 立て直した engine は既定（model=a, thought_level=low）に戻っている
    f.host.restart().await.unwrap();
    let view = f.host.config_view().unwrap();
    assert_eq!(
        view.options
            .iter()
            .map(|o| o.current_value.as_str())
            .collect::<Vec<_>>(),
        ["b", "high"],
        "利用者が選んだ値に当て直す"
    );
    let sets = f.params_of("session/set_config_option");
    assert_eq!(sets.len(), 4, "{sets:?}");
    assert_eq!(
        (&sets[2]["configId"], &sets[2]["value"]),
        (&json!("model"), &json!("b"))
    );
    assert_eq!(
        (&sets[3]["configId"], &sets[3]["value"]),
        (&json!("thought_level"), &json!("high"))
    );
}

/// 立て直しが失敗したら、`restart()` も `history()` も Err。終了を event で出す。
#[tokio::test]
async fn failed_restart_is_reported_everywhere() {
    let mut f = start(
        vec![
            Script::default(),
            Script {
                load_error: true,
                ..Script::default()
            },
        ],
        HostOptions::default(),
    );
    f.until(|e| matches!(e, E::SessionInit { .. })).await;
    let error = f.host.restart().await.unwrap_err();
    assert!(matches!(error, HostError::Establish(_)), "{error:?}");
    assert_eq!(f.host.history().await, Err(error));
    f.until(|e| matches!(e, E::EngineExited { .. })).await;
}

/// アプリが渡した関数が panic しても、待っている呼び手は Err を受け取る（永久に待たない）。
#[tokio::test]
async fn panic_in_injected_callback_does_not_hang_callers() {
    let mut f = start(
        vec![Script::default()],
        HostOptions {
            on_session: Arc::new(|_| panic!("アプリの callback の不具合")),
            ..HostOptions::default()
        },
    );
    let result = tokio::time::timeout(WAIT, f.host.history())
        .await
        .expect("history() が返る");
    assert!(matches!(result, Err(HostError::Dead(_))), "{result:?}");
    f.until(|e| matches!(e, E::EngineExited { .. })).await;
    assert!(f.host.session_init().is_none(), "poison された後も読める");
}

/// `stop()` は終了の event を出さず、その後の呼び出しは Err。
#[tokio::test]
async fn stop_is_quiet_and_final() {
    let mut f = start(vec![Script::default()], HostOptions::default());
    f.until(|e| matches!(e, E::SessionInit { .. })).await;
    f.host.stop();
    assert!(matches!(
        f.host.history().await,
        Ok(_) | Err(HostError::Dead(_))
    ));
    tokio::time::timeout(WAIT, async {
        while f.host.submit("x", &[]).is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("停止の後の submit は Err");
    assert!(matches!(f.host.history().await, Err(HostError::Dead(_))));
    while let Ok(event) = f.events.try_recv() {
        assert!(
            !matches!(event, E::EngineExited { .. }),
            "止めただけで終了の event は出さない"
        );
    }
}

/// host が prompt を出していない時に来た user の発話は、他の書き手の分として出す。
#[tokio::test]
async fn user_message_outside_own_turn_is_broadcast() {
    let script = Script {
        after_load: vec![user_text("他の書き手から"), agent_text("答え")],
        ..Script::default()
    };
    let mut f = start(vec![script], resume("s-1"));
    let events = f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    assert!(events.contains(&user("他の書き手から")), "{events:?}");
    assert_eq!(
        f.host.history().await.unwrap().events,
        [user("他の書き手から"), msg("答え")]
    );
}

/// turn の最中に開き直す: 控えと、その続きの受け口を一緒に取る。抜けも重なりも無い。
#[tokio::test]
async fn attach_returns_history_and_only_later_events() {
    let turn = Turn {
        updates: vec![agent_text("前")],
        wait_cancel: true,
        ..Turn::default()
    };
    let mut f = start(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("hi", &[]).unwrap();
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    let (history, mut later) = f.host.attach().await.unwrap();
    assert_eq!(history.events, [user("hi"), msg("前")]);
    assert!(history.in_flight);
    f.host.interrupt().unwrap();
    let next = tokio::time::timeout(WAIT, later.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(next, E::TurnCompleted { .. }),
        "控えに入った分は受け口に出ない: {next:?}"
    );
}

/// 送ったばかりで応答がまだ何も無い turn の最中も、立て直さない。
#[tokio::test]
async fn restart_is_busy_while_a_turn_is_queued() {
    let turns = [Turn {
        wait_cancel: true,
        ..Turn::default()
    }];
    let mut f = start(
        vec![Script {
            turns: turns.into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.until(|e| matches!(e, E::SessionInit { .. })).await;
    f.host.submit("a", &[]).unwrap();
    assert_eq!(f.host.restart().await, Err(HostError::Busy));
    f.host.interrupt().unwrap();
    f.until_turn_end().await;
}

#[cfg(unix)]
mod process {
    use super::*;
    use agent_chat_acp::LaunchContext;

    fn sh(script: &str) -> LaunchContext {
        LaunchContext {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env: vec![("ACP_TEST_ADDED".into(), "yes".into())],
            env_remove: vec!["ACP_TEST_REMOVED".into()],
        }
    }

    #[tokio::test]
    async fn missing_program_is_a_spawn_error() {
        let launch = LaunchContext {
            program: PathBuf::from("/no/such/acp-engine"),
            ..sh("")
        };
        let error = AcpClientHost::spawn(launch, HostOptions::default())
            .err()
            .expect("起動の失敗");
        assert!(
            matches!(&error, HostError::Spawn(m) if m.contains("/no/such/acp-engine")),
            "{error:?}"
        );
    }

    /// engine がすぐ終了したら、stderr の末尾を添えて知らせる。cwd と env も渡っている。
    #[tokio::test]
    async fn early_exit_reports_stderr_tail_with_launch_context() {
        // SAFETY: この test だけが触る名前。子 process に継承されないことを確かめるために置く。
        unsafe { std::env::set_var("ACP_TEST_REMOVED", "leak") };
        let host = AcpClientHost::spawn(
            sh("echo \"cwd=$(pwd -P) added=$ACP_TEST_ADDED removed=${ACP_TEST_REMOVED:-none}\" >&2; exit 3"),
            HostOptions::default(),
        )
        .expect("起動");
        let mut events = host.subscribe();
        let error = tokio::time::timeout(WAIT, host.history())
            .await
            .unwrap()
            .unwrap_err();
        let HostError::Establish(message) = &error else {
            panic!("{error:?}")
        };
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        assert!(
            message.contains(&format!("cwd={}", cwd.display())),
            "{message}"
        );
        assert!(message.contains("added=yes removed=none"), "{message}");
        let event = tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, E::EngineExited { .. }), "{event:?}");
    }
}

/// 設定の当て直しに engine が答えなくても、会話は使える。確定の上限には数えない。
#[tokio::test]
async fn unanswered_config_reapply_does_not_stop_the_host() {
    let script = Script {
        config: config(),
        config_hang: true,
        turns: [Turn {
            updates: vec![agent_text("ok")],
            ..Turn::default()
        }]
        .into(),
        ..Script::default()
    };
    let mut f = start(
        vec![script],
        HostOptions {
            // 当て直しの打ち切りの方が、確定の上限より長い
            establish_timeout: Duration::from_millis(100),
            config_timeout: Duration::from_millis(300),
            config_intent: vec![
                ("model".into(), "b".into()),
                ("thought_level".into(), "high".into()),
            ],
            ..HostOptions::default()
        },
    );
    f.host.submit("hi", &[]).unwrap();
    let events = f.until_turn_end().await;
    assert!(
        events.contains(&msg("ok")),
        "打ち切った後に発話を送る: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, E::EngineExited { .. })),
        "{events:?}"
    );
    let view = f.host.config_view().unwrap();
    assert!(view.error.is_some(), "打ち切ったことを残す");
    assert_eq!(view.options[0].current_value, "a");
    assert_eq!(
        f.params_of("session/set_config_option").len(),
        1,
        "答えが無ければ、残りの軸は送らずに打ち切る"
    );
}
