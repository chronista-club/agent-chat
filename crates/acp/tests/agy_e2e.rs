//! 実機 E2E: 本物の `agy acp`（Google Antigravity CLI）を host で動かす。
//!
//! CI では走らない（agy の CLI と認証が要り、model の使用枠を使う）。手元で:
//!
//! ```sh
//! cargo test -p agent-chat-acp --test agy_e2e -- --ignored --nocapture --test-threads=1
//! ```
//!
//! model は `gemini-3.8-flash`（枠は model ごとに別。既定の model は枠が切れていることがある）。
//! 各テストは試験用の会話を agy に 1 つ作る（cwd は temp の下）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_chat_acp::{
    AcpClientHost, HistorySnapshot, HostOptions, LaunchContext, PermissionDecision, PromptImage,
};
use agent_chat_protocol::ConversationEvent as E;
use tokio::sync::broadcast;

const MODEL: &str = "gemini-3.8-flash";
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
/// 1x1 の PNG。
const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

fn workdir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("agent-chat-acp-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("作業ディレクトリ");
    dir.canonicalize().expect("絶対 path")
}

fn launch(cwd: &Path, skip_permissions: bool) -> LaunchContext {
    let mut args = vec!["acp".to_string()];
    if skip_permissions {
        args.push("--dangerously-skip-permissions".to_string());
    }
    LaunchContext {
        program: PathBuf::from(std::env::var("AGY_BIN").unwrap_or_else(|_| "agy".to_string())),
        args,
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
        env_remove: Vec::new(),
    }
}

fn options(resume: Option<String>) -> HostOptions {
    HostOptions {
        resume,
        config_intent: vec![("model".to_string(), MODEL.to_string())],
        ..HostOptions::default()
    }
}

/// event の種類を 1 文字にする（本文を出さずに並びを見る）。
fn kind(event: &E) -> &'static str {
    match event {
        E::SessionInit { .. } => "Init",
        E::UserMessage { .. } => "U",
        E::MessageChunk { .. } => "A",
        E::ThoughtChunk { .. } => "t",
        E::ToolCall { .. } => "c",
        E::ToolCallUpdate { .. } => "u",
        E::Plan { .. } => "P",
        E::TurnCompleted { .. } => "End",
        E::Error { .. } => "Err",
        E::EngineExited { .. } => "Exit",
        E::Notice { .. } => "Notice",
        _ => "?",
    }
}

fn kinds(events: &[E]) -> String {
    events.iter().map(kind).collect::<Vec<_>>().join(" ")
}

fn text_of(events: &[E]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            E::MessageChunk { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn turn(events: &mut broadcast::Receiver<E>) -> Vec<E> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(TURN_TIMEOUT, events.recv())
            .await
            .unwrap_or_else(|_| panic!("turn が終わらない。ここまで: {}", kinds(&out)))
            .expect("broadcast");
        let done = matches!(event, E::TurnCompleted { .. } | E::EngineExited { .. });
        out.push(event);
        if done {
            return out;
        }
    }
}

/// turn の終わりの後、しばらく待って遅れて来た event を集める。
async fn stragglers(events: &mut broadcast::Receiver<E>) -> Vec<E> {
    let mut out = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
        out.push(event);
    }
    out
}

fn session_id(host: &AcpClientHost) -> String {
    match host.session_init() {
        Some(E::SessionInit { session_id, .. }) => session_id,
        other => panic!("SessionInit が無い: {other:?}"),
    }
}

fn describe(label: &str, history: &HistorySnapshot) {
    println!(
        "{label}: {} | in_flight={} truncated={}",
        kinds(&history.events),
        history.in_flight,
        history.truncated
    );
}

/// 新規 → turn → 止める → 再開（load）→ 立て直し。履歴が engine から戻ること。
#[tokio::test]
#[ignore = "実機の agy と認証が要る"]
async fn e2e_new_turn_then_load_and_restart_restore_history() {
    let cwd = workdir("load");
    let host = AcpClientHost::spawn(launch(&cwd, true), options(None)).expect("起動");
    let mut events = host.subscribe();
    let started = Instant::now();
    host.history().await.expect("会話の確定");
    println!("確定まで {:?}", started.elapsed());
    let sid = session_id(&host);
    println!("session {sid} / SessionInit: {:?}", host.session_init());
    println!(
        "config: {:?}",
        host.config_view().map(|v| (
            v.options
                .iter()
                .map(|o| format!("{}={}", o.id, o.current_value))
                .collect::<Vec<_>>(),
            v.error
        ))
    );

    host.submit("Reply with only the word: pong", &[]).unwrap();
    let first = turn(&mut events).await;
    println!("turn 1: {}", kinds(&first));
    assert!(
        text_of(&first).to_lowercase().contains("pong"),
        "{}",
        text_of(&first)
    );
    assert!(
        matches!(
            first.last(),
            Some(E::TurnCompleted {
                context_tokens: Some(_),
                ..
            })
        ),
        "{:?}",
        first.last()
    );
    println!(
        "turn 1 の後に遅れて来た event: {}",
        kinds(&stragglers(&mut events).await)
    );

    host.submit("Reply with only the word: ping", &[]).unwrap();
    let second = turn(&mut events).await;
    println!("turn 2: {}", kinds(&second));
    let live = host.history().await.unwrap();
    describe("会話中の控え", &live);
    host.stop();
    drop(host);

    let host =
        AcpClientHost::spawn(launch(&cwd, true), options(Some(sid.clone()))).expect("再起動");
    let mut events = host.subscribe();
    let started = Instant::now();
    let loaded = host.history().await.expect("load");
    println!("load で確定まで {:?}", started.elapsed());
    describe("load の控え", &loaded);
    assert_eq!(session_id(&host), sid);
    let users = |h: &HistorySnapshot| {
        h.events
            .iter()
            .filter_map(|e| match e {
                E::UserMessage { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        users(&loaded),
        users(&live),
        "user の発話が engine から戻る"
    );
    assert_eq!(
        text_of(&loaded.events),
        text_of(&live.events),
        "本文が engine から戻る"
    );
    println!(
        "config（load と当て直しの後）: {:?}",
        host.config_view().map(|v| (
            v.options
                .iter()
                .map(|o| format!("{}={}", o.id, o.current_value))
                .collect::<Vec<_>>(),
            v.error
        ))
    );

    let started = Instant::now();
    let restarted = host.restart().await.expect("立て直し");
    println!("立て直しで確定まで {:?}", started.elapsed());
    assert_eq!(restarted.events, loaded.events);

    host.submit(
        "What two words did I ask you to reply with so far? Answer with just the two words.",
        &[],
    )
    .unwrap();
    let third = turn(&mut events).await;
    println!(
        "turn 3（立て直しの後）: {} / {}",
        kinds(&third),
        text_of(&third)
    );
    let answer = text_of(&third).to_lowercase();
    assert!(
        answer.contains("pong") && answer.contains("ping"),
        "立て直しの後も文脈が続く: {answer}"
    );
    host.stop();
}

/// 中断: 長い turn を `session/cancel` で止める。
#[tokio::test]
#[ignore = "実機の agy と認証が要る"]
async fn e2e_interrupt_ends_the_turn() {
    let cwd = workdir("cancel");
    let host = AcpClientHost::spawn(launch(&cwd, true), options(None)).expect("起動");
    let mut events = host.subscribe();
    host.history().await.expect("会話の確定");
    host.submit("Write the numbers from 1 to 400, one per line, with a short English sentence after each number.", &[]).unwrap();
    let mut before = Vec::new();
    loop {
        let event = tokio::time::timeout(TURN_TIMEOUT, events.recv())
            .await
            .expect("本文を待つ")
            .expect("broadcast");
        let is_text = matches!(event, E::MessageChunk { .. });
        before.push(event);
        if is_text {
            break;
        }
    }
    let started = Instant::now();
    host.interrupt().unwrap();
    let after = turn(&mut events).await;
    println!(
        "中断の前: {} / 中断の後: {} / 終わるまで {:?}",
        kinds(&before),
        kinds(&after),
        started.elapsed()
    );
    assert!(
        matches!(after.last(), Some(E::TurnCompleted { .. })),
        "{}",
        kinds(&after)
    );
    println!(
        "中断の後に遅れて来た event: {}",
        kinds(&stragglers(&mut events).await)
    );
    let sid = session_id(&host);
    describe("中断した turn の控え", &host.history().await.unwrap());
    host.stop();
    drop(host);

    // 中断した turn が load の流れでどう見えるか
    let host = AcpClientHost::spawn(launch(&cwd, true), options(Some(sid))).expect("再起動");
    describe(
        "中断した turn の load",
        &host.history().await.expect("load"),
    );
    host.stop();
}

/// tool と permission: skip の flag なしで、permission の要求に方針で答える。
#[tokio::test]
#[ignore = "実機の agy と認証が要る"]
async fn e2e_tool_call_with_permission_policy() {
    let cwd = workdir("tool");
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&asked);
    let host = AcpClientHost::spawn(
        launch(&cwd, false),
        HostOptions {
            permission: Arc::new(move |request| {
                let kinds: Vec<String> = request
                    .options
                    .iter()
                    .map(|o| format!("{:?}", o.kind))
                    .collect();
                sink.lock().unwrap().push(kinds);
                agent_chat_acp::permission::allow(request)
            }),
            ..options(None)
        },
    )
    .expect("起動");
    let mut events = host.subscribe();
    host.history().await.expect("会話の確定");
    host.submit(
        "Create a file named hello.txt in the current directory containing exactly: hi",
        &[],
    )
    .unwrap();
    let events_seen = turn(&mut events).await;
    println!("tool の turn: {}", kinds(&events_seen));
    println!(
        "permission の要求（option の kind）: {:?}",
        asked.lock().unwrap()
    );
    for event in &events_seen {
        if let E::ToolCall { name, .. } = event {
            println!("  tool: {name}");
        }
        if let E::ToolCallUpdate {
            content, is_error, ..
        } = event
        {
            println!("  結果: {} bytes, is_error={is_error}", content.len());
        }
    }
    assert!(
        events_seen.iter().any(|e| matches!(e, E::ToolCall { .. })),
        "tool が呼ばれる"
    );
    assert_eq!(
        std::fs::read_to_string(cwd.join("hello.txt"))
            .map(|s| s.trim().to_string())
            .ok()
            .as_deref(),
        Some("hi")
    );
    let _ = PermissionDecision::Cancelled;
    host.stop();
}

/// 設定の切替と画像つきの発話。画像つきの発話が load の流れでどう見えるか。
#[tokio::test]
#[ignore = "実機の agy と認証が要る"]
async fn e2e_config_switch_and_image_prompt() {
    let cwd = workdir("image");
    let host = AcpClientHost::spawn(launch(&cwd, true), options(None)).expect("起動");
    let mut events = host.subscribe();
    host.history().await.expect("会話の確定");
    let view = host.config_view().expect("設定面");
    println!(
        "軸: {:?}",
        view.options
            .iter()
            .map(|o| (o.id.clone(), o.current_value.clone(), o.choices.len()))
            .collect::<Vec<_>>()
    );
    if let Some(axis) = view.options.iter().find(|o| o.id == "thought_level")
        && let Some(choice) = axis.choices.iter().find(|c| c.value != axis.current_value)
    {
        let options = host
            .set_config("thought_level", &choice.value)
            .await
            .expect("切替");
        println!(
            "thought_level → {}: {:?}",
            choice.value,
            options
                .iter()
                .map(|o| format!("{}={}", o.id, o.current_value))
                .collect::<Vec<_>>()
        );
    }
    let bad = host.set_config("model", "no-such-model").await;
    println!(
        "不正な値: {bad:?} / view.error: {:?}",
        host.config_view().and_then(|v| v.error)
    );
    assert!(bad.is_err());

    let image = PromptImage {
        media_type: "image/png".into(),
        data: PIXEL_PNG.into(),
    };
    host.submit(
        "This image is a single pixel. Reply with only one word: its approximate color.",
        &[image],
    )
    .unwrap();
    let seen = turn(&mut events).await;
    println!("画像の turn: {} / {}", kinds(&seen), text_of(&seen));
    assert!(matches!(seen.last(), Some(E::TurnCompleted { .. })));
    assert!(
        !seen.iter().any(|e| matches!(e, E::Error { .. })),
        "{seen:?}"
    );
    let sid = session_id(&host);
    host.stop();
    drop(host);

    let host = AcpClientHost::spawn(launch(&cwd, true), options(Some(sid))).expect("再起動");
    let loaded = host.history().await.expect("load");
    describe("画像つきの発話の load", &loaded);
    for event in &loaded.events {
        if let E::UserMessage { text } = event {
            println!("  user の発話: {} bytes", text.len());
        }
    }
    host.stop();
}

/// 実物の大きい会話を load して、流れを全部控えに読み切れているかを数える（読むだけ）。
///
/// `ACP_E2E_LOAD_SESSION`（sessionId）と `ACP_E2E_LOAD_CWD`（その会話の cwd）を渡す。
/// 設定の当て直しはしない（実物の会話の設定を動かさない）。
#[tokio::test]
#[ignore = "実機の agy と、load する実物の会話が要る"]
async fn e2e_load_real_conversation_counts() {
    let (Ok(session), Ok(cwd)) = (
        std::env::var("ACP_E2E_LOAD_SESSION"),
        std::env::var("ACP_E2E_LOAD_CWD"),
    ) else {
        println!("ACP_E2E_LOAD_SESSION / ACP_E2E_LOAD_CWD が無いので何もしない");
        return;
    };
    let cwd = PathBuf::from(cwd);
    let count = |events: &[E]| {
        let n = |f: fn(&E) -> bool| events.iter().filter(|e| f(e)).count();
        format!(
            "U={} t={} A={} c={} u={} 計={}",
            n(|e| matches!(e, E::UserMessage { .. })),
            n(|e| matches!(e, E::ThoughtChunk { .. })),
            n(|e| matches!(e, E::MessageChunk { .. })),
            n(|e| matches!(e, E::ToolCall { .. })),
            n(|e| matches!(e, E::ToolCallUpdate { .. })),
            events.len()
        )
    };
    let bytes = |events: &[E]| -> usize {
        events
            .iter()
            .map(|e| serde_json::to_vec(e).map_or(0, |v| v.len()))
            .sum()
    };
    for (label, limits) in [
        (
            "上限なし",
            agent_chat_acp::HistoryLimits {
                max_bytes: usize::MAX,
                max_events: usize::MAX,
                max_item_bytes: usize::MAX,
            },
        ),
        ("既定の上限", agent_chat_acp::HistoryLimits::default()),
    ] {
        let host = AcpClientHost::spawn(
            launch(&cwd, true),
            HostOptions {
                resume: Some(session.clone()),
                history_limits: limits,
                ..HostOptions::default()
            },
        )
        .expect("起動");
        let started = Instant::now();
        let (history, mut later) = host.attach().await.expect("load");
        println!(
            "{label}: {} / {} bytes / truncated={} / 確定まで {:?}",
            count(&history.events),
            bytes(&history.events),
            history.truncated,
            started.elapsed()
        );
        // 読み切れていなければ、残りが会話中の event として受け口に出る
        let mut leaked = Vec::new();
        while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(3), later.recv()).await {
            leaked.push(event);
        }
        println!("{label}: 確定の後に受け口に出た event: {}", kinds(&leaked));
        assert!(
            leaked.iter().all(|e| matches!(e, E::SessionInit { .. })),
            "load の流れが会話中の event として漏れている: {}",
            kinds(&leaked)
        );
        host.stop();
    }
}
