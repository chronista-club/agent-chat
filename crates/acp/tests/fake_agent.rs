//! 偽の agent（メモリの中の JSON-RPC の相手）で host を確かめる。
//!
//! 実 CLI 無しで、load の流れ・turn・中断・設定の切替・知らない通知・EOF・立て直しを決まった形で再現する。
//! 相手は SDK の `Channel::duplex()` の片側で、生の JSON-RPC を台本どおりに返す。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// updates を送った後、応答までこの時間だけ待つ（turn の最中を作る）。
    delay: Duration,
    /// 応答の前に permission を求める（options）。
    permission: Option<Value>,
    /// prompt の応答を error にする。
    error: bool,
    /// updates を送った後、応答せずに接続を閉じる。
    eof: bool,
    /// updates を送った後、応答しない。`session/cancel` にも答えない（固まった engine）。
    hang: bool,
}

/// 接続 1 本ぶんの台本。
#[derive(Clone, Default)]
struct Script {
    /// initialize に答えない。
    hang: bool,
    /// 何も読まずに接続を閉じる（起動してすぐ終了した engine）。
    gone: bool,
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
    if script.gone {
        return;
    }
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
                        if turn.hang {
                            continue;
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
                        if !turn.delay.is_zero() {
                            tokio::time::sleep(turn.delay).await;
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
    /// process を持つ connector のふりをする時の pid（接続のたびに 1 ずつ増やす）。
    pid: Option<u32>,
    /// `shutdown` が呼ばれた回数（process を止めたか）。
    shutdowns: Arc<AtomicUsize>,
    /// stderr の読み切りにかかる時間。
    drain: Duration,
}

impl Connector for FakeConnector {
    type Transport = Channel;
    fn connect(&mut self) -> Result<Channel, String> {
        let script = self.scripts.pop_front().ok_or("台本がもう無い")?;
        let (client, agent) = Channel::duplex();
        tokio::spawn(fake_agent(agent, script, Arc::clone(&self.log)));
        self.pid = self.pid.map(|pid| pid + 1);
        Ok(client)
    }
    fn pid(&self) -> Option<u32> {
        self.pid
    }
    fn shutdown(&mut self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
    fn stderr_drained(&mut self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(self.drain))
    }
}

struct Fixture {
    host: AcpClientHost,
    events: broadcast::Receiver<E>,
    log: Log,
    shutdowns: Arc<AtomicUsize>,
}

fn start(scripts: Vec<Script>, options: HostOptions) -> Fixture {
    start_with_pid(scripts, options, None)
}

fn start_with_pid(scripts: Vec<Script>, options: HostOptions, pid: Option<u32>) -> Fixture {
    start_full(scripts, options, pid, Duration::ZERO)
}

fn start_full(
    scripts: Vec<Script>,
    options: HostOptions,
    pid: Option<u32>,
    drain: Duration,
) -> Fixture {
    let log = Log::default();
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let connector = FakeConnector {
        scripts: scripts.into(),
        log: Arc::clone(&log),
        pid,
        shutdowns: Arc::clone(&shutdowns),
        drain,
    };
    let host = AcpClientHost::spawn_with(connector, PathBuf::from("/work"), options).expect("起動");
    let events = host.subscribe();
    Fixture {
        host,
        events,
        log,
        shutdowns,
    }
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
            delay: Duration::from_millis(200),
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

impl Fixture {
    /// process が止められる（`shutdown` が呼ばれる）まで待つ。
    async fn until_shutdown(&self, within: Duration) {
        tokio::time::timeout(within, async {
            while self.shutdowns.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("process が止められない");
    }
}

/// `stop()` は、会話の確定を待っている最中でも、すぐに process を止める。
#[tokio::test]
async fn stop_before_establish_stops_the_process_at_once() {
    let f = start(
        vec![Script {
            hang: true,
            ..Script::default()
        }],
        HostOptions::default(), // 確定の上限は 30 秒
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    f.host.stop();
    f.until_shutdown(Duration::from_secs(1)).await;
    assert!(f.host.is_finished());
    assert!(matches!(f.host.history().await, Err(HostError::Dead(_))));
}

/// 設定の当て直しを待っている最中の `stop()` も、すぐに process を止める。
#[tokio::test]
async fn stop_during_config_reapply_stops_the_process_at_once() {
    let f = start(
        vec![Script {
            config: config(),
            config_hang: true,
            ..Script::default()
        }],
        HostOptions {
            config_intent: vec![("model".into(), "b".into())],
            ..HostOptions::default() // 当て直しの上限は 15 秒
        },
    );
    f.host.history().await.unwrap();
    assert!(!f.host.is_finished());
    f.host.stop();
    f.until_shutdown(Duration::from_secs(1)).await;
}

/// host を drop しても同じ。確定の前でも process を残さない。
#[tokio::test]
async fn dropping_the_host_before_establish_stops_the_process() {
    let Fixture {
        host, shutdowns, ..
    } = start(
        vec![Script {
            hang: true,
            ..Script::default()
        }],
        HostOptions::default(),
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(host);
    tokio::time::timeout(Duration::from_secs(1), async {
        while shutdowns.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("drop で process が止められる");
}

/// 確定の前に発話を積んでから止めたら、その発話は送らない（止めたはずの engine に送らない）。
#[tokio::test]
async fn stop_wins_over_prompts_submitted_before_it() {
    let f = start(
        vec![Script {
            turns: [Turn::default()].into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("送られないはず", &[]).unwrap();
    f.host.stop();
    f.until_shutdown(Duration::from_secs(1)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !f.methods().contains(&"session/prompt".to_string()),
        "{:?}",
        f.methods()
    );
}

/// engine が終了したら、stderr の読み切りを待つ間も `submit` は Err を返す
/// （Ok を返して黙って捨てない。呼び手は Err を見て host を作り直す）。
#[tokio::test]
async fn submit_fails_as_soon_as_the_engine_is_gone() {
    let turn = Turn {
        eof: true,
        ..Turn::default()
    };
    let mut f = start_full(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions::default(),
        None,
        Duration::from_millis(800), // stderr の読み切りに時間がかかる
    );
    f.until(|e| matches!(e, E::SessionInit { .. })).await;
    f.host.submit("hi", &[]).unwrap();
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_millis(400), async {
        while f.host.submit("again", &[]).is_ok() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("読み切りを待たずに Err になる");
    assert!(f.host.is_finished(), "終了したことも読める");
    f.until(|e| matches!(e, E::EngineExited { .. })).await;
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "終了の文は読み切りの後"
    );
}

/// 会話を確定できなかった時は、待っていた発話が無くても、理由を `Error` で会話に出す
/// （`EngineExited` の文は GUI に出ないことがある。chat を開いただけの人にも理由が見える）。
#[tokio::test]
async fn failed_establish_always_reports_an_error() {
    let mut f = start(
        vec![Script {
            load_error: true,
            ..Script::default()
        }],
        resume("gone"),
    );
    let events = f.until(|e| matches!(e, E::EngineExited { .. })).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(
        matches!(&events[0], E::Error { message }
            if message.contains("会話を確定できませんでした") && message.contains("session/load") && !message.contains("件")),
        "{events:?}"
    );
}

/// 途中から書き込みが失敗するようになる接続（確定の後に、接続そのものが失敗で終わる場面を作る）。
struct FlakyWriter {
    inner: tokio_util::compat::Compat<tokio::io::DuplexStream>,
    broken: Arc<std::sync::atomic::AtomicBool>,
}

impl futures::AsyncWrite for FlakyWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.broken.load(Ordering::SeqCst) {
            return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.broken.load(Ordering::SeqCst) {
            return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_close(cx)
    }
}

struct FlakyConnector {
    broken: Arc<std::sync::atomic::AtomicBool>,
}

impl Connector for FlakyConnector {
    type Transport = agent_client_protocol::ByteStreams<
        FlakyWriter,
        tokio_util::compat::Compat<tokio::io::DuplexStream>,
    >;
    fn connect(&mut self) -> Result<Self::Transport, String> {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
        let (client_out, agent_in) = tokio::io::duplex(64 * 1024);
        let (agent_out, client_in) = tokio::io::duplex(64 * 1024);
        // JSON の行で話す最小の agent。prompt には答えない（turn の最中のままにする）。
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(agent_in).lines();
            let mut out = agent_out;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let result = match message["method"].as_str() {
                    Some("initialize") => json!({"protocolVersion": 1, "agentCapabilities": {}}),
                    Some("session/new") => json!({"sessionId": "new-1"}),
                    _ => continue,
                };
                let reply = json!({"jsonrpc": "2.0", "id": message["id"], "result": result});
                if out
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok(agent_client_protocol::ByteStreams::new(
            FlakyWriter {
                inner: client_out.compat_write(),
                broken: Arc::clone(&self.broken),
            },
            client_in.compat(),
        ))
    }
}

/// 確定の後に接続そのものが失敗で終わっても（書き込みの失敗）、待たせていた発話を数えて知らせる。
#[tokio::test]
async fn prompts_lost_to_a_broken_connection_are_reported() {
    let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let host = AcpClientHost::spawn_with(
        FlakyConnector {
            broken: Arc::clone(&broken),
        },
        PathBuf::from("/work"),
        HostOptions::default(),
    )
    .expect("起動");
    let mut events = host.subscribe();
    host.history().await.expect("確定");
    host.submit("turn の最中", &[]).unwrap();
    host.submit("待ち 1", &[]).unwrap();
    host.submit("待ち 2", &[]).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(host.history().await.unwrap().in_flight);

    // ここから書き込みが失敗する。次に何かを書いた時に接続が失敗で終わる
    broken.store(true, Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_millis(300), host.set_config("model", "x")).await;

    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap_or_else(|_| panic!("終了を待ち切れない: {seen:?}"))
            .unwrap();
        let done = matches!(event, E::EngineExited { .. });
        seen.push(event);
        if done {
            break;
        }
    }
    assert!(
        seen.iter()
            .any(|e| matches!(e, E::Error { message } if message.contains("2 件"))),
        "{seen:?}"
    );
}

/// 会話を確定できなかった時、待っていた発話があれば、送られなかったことを `Error` で出す。
#[tokio::test]
async fn prompts_lost_to_a_failed_establish_are_reported() {
    let mut f = start(
        vec![Script {
            load_error: true,
            ..Script::default()
        }],
        resume("gone"),
    );
    f.host.submit("a", &[]).unwrap();
    f.host.submit("b", &[]).unwrap();
    let events = f.until(|e| matches!(e, E::EngineExited { .. })).await;
    let n = events.len();
    assert!(
        matches!(&events[n - 2], E::Error { message } if message.contains("session/load") && message.contains("2 件")),
        "{events:?}"
    );
}

/// 確定の後に engine が終了した時も、待たせていた発話があれば知らせる。
#[tokio::test]
async fn prompts_lost_to_an_engine_exit_are_reported() {
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
    f.host.submit("first", &[]).unwrap();
    f.host.submit("待ち", &[]).unwrap();
    let events = f.until(|e| matches!(e, E::EngineExited { .. })).await;
    let n = events.len();
    assert!(
        matches!(&events[n - 2], E::Error { message } if message.contains("1 件")),
        "{events:?}"
    );
}

/// engine が標準に無い kind の permission を求めてきたら、型で読めない。黙って待たせず、
/// 未対応として断る（turn は engine の側で続く）。
#[tokio::test]
async fn permission_with_unknown_kind_is_refused_not_ignored() {
    let options = json!([{"optionId": "x", "name": "X", "kind": "allow_for_session"}]);
    let mut f = start(
        vec![Script {
            turns: [Turn {
                permission: Some(options),
                ..Turn::default()
            }]
            .into(),
            ..Script::default()
        }],
        HostOptions::default(),
    );
    f.host.submit("hi", &[]).unwrap();
    f.until_turn_end().await;
    let answer = &f.params_of("permission_response")[0];
    assert!(answer.get("error").is_some(), "{answer}");
}

/// 会話の確定の前に送った発話は、まだ engine に送っていなくても、控えの末尾に入っている。
/// 呼び手は控えを `ReplayStart` 〜 `ReplayEnd` で配って画面を作り直すので、控えに無い発話は画面から消える。
/// 設定の当て直しを待っている間（確定の後、発話を送る前）に控えを取る場面で確かめる。
#[tokio::test]
async fn prompts_submitted_before_they_are_sent_appear_in_history() {
    let script = Script {
        replay: vec![user_text("前の質問"), agent_text("前の答え")],
        config: config(),
        config_hang: true,
        turns: [Turn {
            updates: vec![agent_text("新しい答え")],
            ..Turn::default()
        }]
        .into(),
        ..Script::default()
    };
    let mut f = start(
        vec![script],
        HostOptions {
            config_intent: vec![("model".into(), "b".into())],
            config_timeout: Duration::from_millis(300),
            ..resume("s-1")
        },
    );
    f.host.submit("確定の前に送った", &[]).unwrap();
    let (history, _later) = f.host.attach().await.unwrap();
    assert_eq!(
        history.events,
        [user("前の質問"), msg("前の答え"), user("確定の前に送った")],
        "まだ送っていない発話も、待ちの順で末尾に入る"
    );
    assert!(history.in_flight, "これから turn が走る");
    assert_eq!(
        f.params_of("session/prompt").len(),
        0,
        "この時点ではまだ送っていない"
    );

    // 送った後は「待ち」から「控え」へ移る。二重にならない
    f.until_turn_end().await;
    let history = f.host.history().await.unwrap();
    assert_eq!(
        history.events,
        [
            user("前の質問"),
            msg("前の答え"),
            user("確定の前に送った"),
            msg("新しい答え")
        ]
    );
    assert!(!history.in_flight);
}

/// turn の最中に積まれた発話も同じ。今の turn の途中の本文の後ろに並ぶ。
/// 中断で捨てた分は、控えから消える。
#[tokio::test]
async fn prompts_queued_during_a_turn_appear_in_history_until_dropped() {
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
    f.host.submit("first", &[]).unwrap();
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    f.host.submit("待ち 1", &[]).unwrap();
    f.host.submit("待ち 2", &[]).unwrap();
    let history = f.host.history().await.unwrap();
    assert_eq!(
        history.events,
        [user("first"), msg("途中"), user("待ち 1"), user("待ち 2")]
    );
    assert!(history.in_flight);

    f.host.interrupt().unwrap();
    f.until_turn_end().await;
    let history = f.host.history().await.unwrap();
    assert_eq!(history.events, [user("first"), msg("途中")]);
    assert!(!history.in_flight);
}

/// 設定の当て直しが無くても同じ。確定の前に送った発話は、控えに 1 回だけ出る
/// （待ちとして末尾に入った後、送られて控えに移っても、2 回にならない）。
#[tokio::test]
async fn a_prompt_never_appears_twice_across_attach_calls() {
    let script = Script {
        replay: vec![user_text("前の質問"), agent_text("前の答え")],
        turns: [
            Turn {
                updates: vec![agent_text("答え 1")],
                delay: Duration::from_millis(100),
                ..Turn::default()
            },
            // 2 つ目の turn は、中断するまで終わらない（確認の間に全部終わってしまわないように）
            Turn {
                updates: vec![agent_text("答え 2")],
                wait_cancel: true,
                ..Turn::default()
            },
        ]
        .into(),
        ..Script::default()
    };
    let mut f = start(vec![script], resume("s-1"));
    f.host.submit("一", &[]).unwrap();
    f.host.submit("二", &[]).unwrap();
    let count = |events: &[E], text: &str| events.iter().filter(|e| **e == user(text)).count();
    // 確定の直後・1 つ目の turn の最中・全部終わった後のどこで取っても、各発話は 1 回ずつ
    for _ in 0..3 {
        let history = f.host.history().await.unwrap();
        assert_eq!(
            (count(&history.events, "一"), count(&history.events, "二")),
            (1, 1),
            "{:?}",
            history.events
        );
        assert!(history.in_flight);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // 1 つ目の turn の終わりを待ち、2 つ目の turn の本文が出てから、2 つ目を終わらせる
    f.until_turn_end().await;
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    let history = f.host.history().await.unwrap();
    assert_eq!(
        (count(&history.events, "一"), count(&history.events, "二")),
        (1, 1),
        "{:?}",
        history.events
    );
    f.host.interrupt().unwrap();
    f.until_turn_end().await;
    let history = f.host.history().await.unwrap();
    assert_eq!(
        history.events,
        [
            user("前の質問"),
            msg("前の答え"),
            user("一"),
            msg("答え 1"),
            user("二"),
            msg("答え 2")
        ]
    );
    assert!(!history.in_flight);
}

/// 中断より後に送った発話は、待ちに残る（中断で消すのは、それより前の待ちだけ）。
#[tokio::test]
async fn prompts_submitted_after_an_interrupt_stay_pending() {
    let turns = [
        Turn {
            updates: vec![agent_text("途中")],
            wait_cancel: true,
            ..Turn::default()
        },
        Turn {
            updates: vec![agent_text("次")],
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
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    f.host.submit("捨てられる", &[]).unwrap();
    f.host.interrupt().unwrap();
    f.host.submit("中断の後", &[]).unwrap();
    f.until_turn_end().await;
    f.until_turn_end().await;
    assert_eq!(
        f.host.history().await.unwrap().events,
        [user("first"), msg("途中"), user("中断の後"), msg("次")]
    );
}

/// 中断したら、待たせていた発話も捨てる（止めたのに次の発話が走り出さない）。
/// 捨てた発話は送られず、控えにも入らない。その後の発話は普通に送れる。
#[tokio::test]
async fn interrupt_drops_queued_prompts() {
    let turns = [
        Turn {
            updates: vec![agent_text("1")],
            wait_cancel: true,
            ..Turn::default()
        },
        Turn {
            updates: vec![agent_text("次")],
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
    f.host.submit("捨てられる 1", &[]).unwrap();
    f.host.submit("捨てられる 2", &[]).unwrap();
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    f.host.interrupt().unwrap();
    f.until_turn_end().await;

    // 待たせていた発話が走り出さないこと: 次に送った発話が 2 つ目の prompt になる
    f.host.submit("after", &[]).unwrap();
    let events = f.until_turn_end().await;
    assert!(events.contains(&msg("次")), "{events:?}");
    let prompts: Vec<_> = f
        .params_of("session/prompt")
        .iter()
        .map(|p| {
            p["prompt"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(prompts, ["first", "after"]);
    assert_eq!(
        f.host.history().await.unwrap().events,
        [user("first"), msg("1"), user("after"), msg("次")],
        "捨てた発話は控えに入らない"
    );
}

/// 中断（`session/cancel`）に engine が答えない時（agy が thinking のまま 3000 秒固まった、
/// 2026-11-24 mako の報告）は、待ち切り（既定 3 秒）で turn を閉じ、engine を止める。
/// 止めたのに「応答中」のまま残り、次の発話も送れなくなる、を起こさない。
/// 閉じる順は `TurnCompleted` → `EngineExited`。`TurnCompleted` は host が終了した後に出す。
/// ⚠️ current_thread の runtime なので、`TurnCompleted` を host の終了より前（await を挟まない所）へ
/// 動かしても、ここの「その時点で終了している」は赤にならない（並びはコードで守っている）。
#[tokio::test(start_paused = true)]
async fn unanswered_interrupt_closes_the_turn_and_stops_the_engine() {
    let turn = Turn {
        updates: vec![agent_text("考え中")],
        hang: true,
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
    let interrupted = tokio::time::Instant::now();
    f.host.interrupt().unwrap();
    // 最初に来る終わりの印が TurnCompleted（engine の終了の前に turn を閉じる）。
    let events = f
        .until(|e| matches!(e, E::TurnCompleted { .. } | E::EngineExited { .. }))
        .await;

    let waited = interrupted.elapsed();
    assert!(
        waited >= Duration::from_secs(3) && waited < Duration::from_secs(4),
        "既定の待ち切りは 3 秒: {waited:?}"
    );
    assert!(
        matches!(events.last(), Some(E::TurnCompleted { session_id, .. }) if session_id == "new-1"),
        "engine の終了の前に turn を閉じる: {events:?}"
    );
    // TurnCompleted を受けた時点で host はもう終了している。それを見て送った次の発話は Err になり、
    // 固まった engine に積まれて黙って失われない（呼び手は Err を見て作り直す）。
    assert!(
        f.host.is_finished(),
        "TurnCompleted の時点で host は終了している"
    );
    assert!(
        f.host.submit("次", &[]).is_err(),
        "turn の終わりを見て送った発話は Err"
    );
    let rest = f.until(|e| matches!(e, E::EngineExited { .. })).await;
    assert!(
        matches!(rest.as_slice(), [E::EngineExited { message }] if message.contains("session/cancel")),
        "待ちの発話が無ければ、続くのは理由つきの終了 1 件だけ: {rest:?}"
    );
    assert_eq!(f.params_of("session/cancel").len(), 1);
    assert_eq!(f.shutdowns.load(Ordering::SeqCst), 1, "process を止める");
    assert!(
        matches!(f.host.history().await, Err(HostError::Dead(_))),
        "終了した host の控えは Dead"
    );
}

/// 期限の待ちの間に受け取った発話（中断の後に送られた分）は送られずに終わる。黙って捨てずに、
/// turn を閉じた後、engine の終了の前に `Error` で件数を知らせる。
#[tokio::test(start_paused = true)]
async fn prompts_submitted_while_waiting_for_the_cancel_are_reported() {
    let turn = Turn {
        updates: vec![agent_text("考え中")],
        hang: true,
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
    f.host.interrupt().unwrap();
    f.host.submit("中断の後の発話", &[]).unwrap();
    let events = f.until(|e| matches!(e, E::EngineExited { .. })).await;

    let n = events.len();
    assert!(n >= 3, "{events:?}");
    assert!(
        matches!(
            &events[n - 3..],
            [
                E::TurnCompleted { .. },
                E::Error { message },
                E::EngineExited { .. },
            ] if message.contains("1 件")
        ),
        "turn を閉じる → 送られなかった件数 → 終了: {events:?}"
    );
    assert_eq!(
        f.params_of("session/prompt").len(),
        1,
        "中断の後の発話は送らない"
    );
}

/// 待ち切りは呼び手が決められる。数え始めは最初の中断で、中断を重ねても延ばさない
/// （「止まらないので停止を連打する」で、いつまでも止まらない、を起こさない）。
#[tokio::test(start_paused = true)]
async fn pressing_interrupt_again_does_not_extend_the_deadline() {
    let turn = Turn {
        updates: vec![agent_text("考え中")],
        hang: true,
        ..Turn::default()
    };
    let mut f = start(
        vec![Script {
            turns: [turn].into(),
            ..Script::default()
        }],
        HostOptions {
            cancel_grace: Duration::from_secs(1),
            ..HostOptions::default()
        },
    );
    f.host.submit("長い仕事", &[]).unwrap();
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    let interrupted = tokio::time::Instant::now();
    f.host.interrupt().unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    f.host.interrupt().unwrap();
    f.until(|e| matches!(e, E::EngineExited { .. })).await;

    let waited = interrupted.elapsed();
    assert!(
        waited >= Duration::from_secs(1) && waited < Duration::from_millis(1600),
        "最初の中断から 1 秒で止める: {waited:?}"
    );
}

/// 中断に答えた turn の後は、待ち切りを次の turn へ持ち越さない（待ち切りより長い turn を止めない）。
#[tokio::test(start_paused = true)]
async fn answered_interrupt_does_not_carry_the_deadline_into_the_next_turn() {
    let turns = [
        Turn {
            updates: vec![agent_text("1")],
            wait_cancel: true,
            ..Turn::default()
        },
        Turn {
            updates: vec![agent_text("2")],
            delay: Duration::from_secs(4),
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
    f.until(|e| matches!(e, E::MessageChunk { .. })).await;
    f.host.interrupt().unwrap();
    f.until_turn_end().await;

    f.host.submit("second", &[]).unwrap();
    let events = f.until_turn_end().await;
    assert!(events.contains(&msg("2")), "{events:?}");
    assert!(
        !events.iter().any(|e| matches!(e, E::EngineExited { .. })),
        "{events:?}"
    );
    assert!(!f.host.is_finished());
}

/// opencode は load で、tool を `tool_call`（pending）と `tool_call_update` の 2 段で流す
/// （agy / grok は結果まで入った 1 件）。控えでは 1 組の `ToolCall` + `ToolCallUpdate` になる。
#[tokio::test]
async fn load_stream_with_pending_tool_call_then_update_is_one_pair() {
    let script = Script {
        replay: vec![
            user_text("file を作って"),
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "status": "pending", "kind": "edit",
                   "name": "write", "title": "write hello.txt", "locations": [{"path": "/w/hello.txt"}],
                   "rawInput": {"path": "hello.txt"}}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                   "locations": [{"path": "/w/hello.txt"}], "rawOutput": {"ok": true},
                   "content": [{"type": "content", "content": {"type": "text", "text": "wrote"}}]}),
            agent_text("作りました"),
        ],
        ..Script::default()
    };
    let f = start(vec![script], resume("s-1"));
    assert_eq!(
        f.host.history().await.unwrap().events,
        [
            user("file を作って"),
            E::ToolCall {
                id: "t1".into(),
                name: "write hello.txt".into(),
                input: json!({"path": "hello.txt"}),
            },
            E::ToolCallUpdate {
                tool_use_id: "t1".into(),
                content: "wrote".into(),
                is_error: false,
            },
            msg("作りました"),
        ]
    );
}

/// 会話が確定する前に接続が切れたら、確定の失敗として返す（確定の後の終了とは分ける）。
/// 切れ方（EOF が先か、書き込みの失敗が先か）は OS と時機で変わる。
#[tokio::test]
async fn connection_lost_before_establish_is_an_establish_error() {
    for _ in 0..20 {
        let f = start(
            vec![Script {
                gone: true,
                ..Script::default()
            }],
            HostOptions::default(),
        );
        let error = tokio::time::timeout(WAIT, f.host.history())
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, HostError::Establish(_)), "{error:?}");
    }
}

/// 書き込みが必ず `Broken pipe` になり、何も読めない接続（initialize を読む前に終了した engine を、
/// 時機に頼らずに再現する。Linux の CI では実 process でこの切れ方になった）。
struct BrokenPipe;

impl futures::AsyncWrite for BrokenPipe {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
    }
    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl futures::AsyncRead for BrokenPipe {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Pending
    }
}

struct BrokenPipeConnector;

impl Connector for BrokenPipeConnector {
    type Transport = agent_client_protocol::ByteStreams<BrokenPipe, BrokenPipe>;
    fn connect(&mut self) -> Result<Self::Transport, String> {
        Ok(agent_client_protocol::ByteStreams::new(
            BrokenPipe, BrokenPipe,
        ))
    }
    fn stderr_tail(&self) -> String {
        "engine の最後の言葉".to_string()
    }
}

/// 確定の前に書き込みが失敗して接続が切れても、確定の失敗として返す。stderr の末尾も付く。
#[tokio::test]
async fn write_failure_before_establish_is_an_establish_error() {
    let host = AcpClientHost::spawn_with(
        BrokenPipeConnector,
        PathBuf::from("/work"),
        HostOptions::default(),
    )
    .expect("起動");
    let error = tokio::time::timeout(WAIT, host.history())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(&error, HostError::Establish(m) if m.contains("engine の最後の言葉")),
        "{error:?}"
    );
}

/// process を持たない connector の pid は None。持つなら、立て直しで新しい pid に替わる。
#[tokio::test]
async fn pid_follows_the_current_connection() {
    let f = start(vec![Script::default()], HostOptions::default());
    f.host.history().await.unwrap();
    assert_eq!(f.host.pid(), None);

    let mut f = start_with_pid(
        vec![Script::default(), Script::default()],
        HostOptions::default(),
        Some(100),
    );
    assert_eq!(f.host.pid(), Some(101), "起動した時点で読める");
    f.until(|e| matches!(e, E::SessionInit { .. })).await;
    f.host.restart().await.unwrap();
    assert_eq!(f.host.pid(), Some(102));
    f.host.stop();
    tokio::time::timeout(WAIT, async {
        while f.host.pid().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("止めたら pid は None");
}

/// 同じ `messageId` の `user_message_chunk` が続いたら 1 つの発話にする（opencode）。
#[tokio::test]
async fn user_chunks_sharing_a_message_id_form_one_history_message() {
    let user_chunk = |id: &str, text: &str| {
        json!({"sessionUpdate": "user_message_chunk", "messageId": id,
               "content": {"type": "text", "text": text}})
    };
    let script = Script {
        replay: vec![
            user_chunk("m1", "he"),
            user_chunk("m1", "llo"),
            agent_text("答え"),
            user_chunk("m2", "x"),
            user_text("y"),
        ],
        ..Script::default()
    };
    let f = start(vec![script], resume("s-1"));
    assert_eq!(
        f.host.history().await.unwrap().events,
        [user("hello"), msg("答え"), user("x"), user("y")]
    );
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
        // 確定の失敗は、理由を Error で出してから EngineExited
        let first = tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first, E::Error { .. }), "{first:?}");
        let second = tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(second, E::EngineExited { .. }), "{second:?}");
    }

    /// pid は起動した child のもの。engine が終了したら None。
    #[tokio::test]
    async fn pid_is_the_child_process_and_clears_on_exit() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let host = AcpClientHost::spawn(
            sh("echo \"pid=$$\" >&2; read line; exit 0"),
            HostOptions {
                on_stderr: Arc::new(move |line| sink.lock().unwrap().push(line.to_string())),
                ..HostOptions::default()
            },
        )
        .expect("起動");
        let pid = host.pid().expect("起動した時点で pid が読める");
        tokio::time::timeout(WAIT, async {
            while lines.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("stderr の行");
        assert_eq!(*lines.lock().unwrap(), [format!("pid={pid}")]);
        // initialize の行を読んだ sh が終了する
        assert!(
            tokio::time::timeout(WAIT, host.history())
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(host.pid(), None);
    }

    /// stderr は 1 行ごとに呼び手に渡す。空行は渡さず、UTF-8 でない byte は置き換える。
    /// 改行で終わらない最後の行も渡す。終了の文に付ける末尾は今までどおり。
    #[tokio::test]
    async fn stderr_lines_reach_the_caller() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let host = AcpClientHost::spawn(
            sh("printf 'one\\n\\n  two\\r\\n\\377x\\nlast' >&2; exit 3"),
            HostOptions {
                on_stderr: Arc::new(move |line| sink.lock().unwrap().push(line.to_string())),
                ..HostOptions::default()
            },
        )
        .expect("起動");
        let error = tokio::time::timeout(WAIT, host.history())
            .await
            .unwrap()
            .unwrap_err();
        tokio::time::timeout(WAIT, async {
            while lines.lock().unwrap().len() < 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("stderr の行: {:?}", lines.lock().unwrap()));
        assert_eq!(
            *lines.lock().unwrap(),
            ["one", "  two", "\u{FFFD}x", "last"]
        );
        assert!(
            matches!(&error, HostError::Establish(m) if m.contains("last")),
            "{error:?}"
        );
    }

    /// 立て直しの後、前の process が遅れて書いた stderr は、新しい process の分に混ざらない。
    /// 前の process の孫が stderr を握ったまま、親が止められた後に書く場面で確かめる。
    #[tokio::test]
    async fn stderr_of_the_previous_process_does_not_leak_into_the_next() {
        use agent_chat_acp::ProcessConnector;
        let marker = std::env::temp_dir().join(format!("acp-stderr-leak-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            "if [ -e '{m}' ]; then echo from-new >&2; exec sleep 30; \
             else : > '{m}'; (sleep 0.3; echo late-from-old >&2) & exec sleep 30; fi",
            m = marker.display()
        );
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let mut connector = ProcessConnector::new(sh(&script)).on_stderr(Arc::new(move |line| {
            sink.lock().unwrap().push(line.to_string())
        }));

        let old = connector.connect().expect("1 つ目");
        // 1 つ目が動き出して孫を起こすまで待つ（すぐ止めると、script が走る前に終わる）
        tokio::time::timeout(WAIT, async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("1 つ目の process が動き出す");
        tokio::time::sleep(Duration::from_millis(50)).await;
        connector.shutdown();
        drop(old);
        let new = connector.connect().expect("2 つ目");
        tokio::time::sleep(Duration::from_millis(800)).await;

        assert_eq!(connector.stderr_tail(), "from-new");
        assert_eq!(*lines.lock().unwrap(), ["from-new"]);
        connector.shutdown();
        drop(new);
        let _ = std::fs::remove_file(&marker);
    }

    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// 実 process でも、会話の確定を待っている最中の `stop()` と drop は process をすぐ止める。
    #[tokio::test]
    async fn stop_and_drop_before_establish_kill_the_real_process() {
        for by_drop in [false, true] {
            // ACP を話さない process。確定の上限（30 秒）まで待たされる相手
            let host =
                AcpClientHost::spawn(sh("exec sleep 30"), HostOptions::default()).expect("起動");
            let pid = host.pid().expect("pid");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(alive(pid), "止める前は生きている");
            if by_drop {
                drop(host);
            } else {
                host.stop();
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                while alive(pid) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("process {pid} が残っている（drop={by_drop}）"));
        }
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
