//! AcpClientHost — ACP engine を公式 SDK（`agent-client-protocol`、protocol v1）で動かす host。
//!
//! 設計は `docs/design/01-acp-client-host.md`。VP の手書きの `AcpAgentHost`
//! （nightly `504859ba` の `conversation/acp_host.rs`）と同じ役割を、共有側で持つ。
//!
//! - process は自分で起動して [`ByteStreams`] に渡す（cwd・env の除去・stderr を扱うため）
//! - 1 つの task が接続と session を持ち、外の呼び手とは mpsc でつなぐ
//! - `session/load` が流す過去の会話は broadcast に流さず、履歴の控えに入れる（[`AcpClientHost::history`]）
//! - 他の書き手が会話を進めたら、[`AcpClientHost::restart`] で process を立て直す
//!
//! tokio の runtime の中で使う（task と process を起動するため）。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_chat_protocol::ConversationEvent;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, ImageContent, InitializeRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigId, SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::v1::{ActiveSession, ConnectionTo, SessionMessage, SessionSetup};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, Dispatch};
use futures::FutureExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::config::{AcpConfigOption, AcpConfigView, needs_apply, parse_config_options};
use crate::history::{History, HistoryLimits, HistorySnapshot};
use crate::permission::{self, PermissionDecision};
use crate::session_meta::{
    extract_available_commands, extract_current_mode, extract_session_meta, extract_usage_update,
};
use crate::translate::AcpTranslator;

/// `set_config_option` の応答を待つ上限（1 軸）。agy の実測の応答は 0.1 秒ほど。
const CONFIG_SET_TIMEOUT: Duration = Duration::from_secs(15);
/// 保持する stderr の末尾の大きさ。
const STDERR_TAIL_BYTES: usize = 4096;
const EVENT_CAPACITY: usize = 1024;

/// 起動の文脈。どの CLI をどう起動するかはアプリが決める。
#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// 会話の作業ディレクトリ（絶対 path）。process の cwd と ACP の `cwd` の両方に使う。
    pub cwd: PathBuf,
    /// 足す env。
    pub env: Vec<(String, String)>,
    /// 継承した env から剥がす名前。
    pub env_remove: Vec<String>,
}

/// `session/load` が失敗した時の方針。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadFallback {
    /// 失敗として返し、host は終了する。黙って新規の会話に変えない。
    #[default]
    Fail,
    /// 新規の会話にする。変えた事実は `Notice` で出す。
    NewSession,
}

pub type PermissionPolicy =
    Arc<dyn Fn(&RequestPermissionRequest) -> PermissionDecision + Send + Sync>;
pub type SessionCallback = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// host の注入点。
#[derive(Clone)]
pub struct HostOptions {
    /// 再開する ACP の sessionId。None なら新規。
    pub resume: Option<String>,
    pub on_load_failed: LoadFallback,
    /// 起動から会話の確定までの上限。
    pub establish_timeout: Duration,
    /// 会話の確定の後に当て直す `(config id, 値)`。agy は load の後に既定へ戻る。
    pub config_intent: Vec<(String, String)>,
    /// permission の要求への答え方。重い処理をしない（接続全体が止まる）。
    pub permission: PermissionPolicy,
    /// 会話が確定した時に sessionId を受け取る（アプリの永続化）。Err は `Error` event で出す。
    pub on_session: SessionCallback,
    pub history_limits: HistoryLimits,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            resume: None,
            on_load_failed: LoadFallback::default(),
            establish_timeout: Duration::from_secs(30),
            config_intent: Vec::new(),
            permission: Arc::new(permission::allow),
            on_session: Arc::new(|_| Ok(())),
            history_limits: HistoryLimits::default(),
        }
    }
}

/// prompt に並べる画像（base64）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptImage {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// process を起動できなかった。
    Spawn(String),
    /// 会話が確定しなかった（initialize / load / new の失敗、上限の超過）。
    Establish(String),
    /// engine が終了している。
    Dead(String),
    /// turn の最中にはできない。
    Busy,
    /// engine が要求を断った。
    Engine(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(m) => write!(f, "engine を起動できません: {m}"),
            Self::Establish(m) => write!(f, "会話を確定できません: {m}"),
            Self::Dead(m) => write!(f, "engine は終了しています: {m}"),
            Self::Busy => write!(f, "turn の最中です"),
            Self::Engine(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for HostError {}

/// 接続の作り方。既定は process の起動（[`ProcessConnector`]）。テストは偽の agent をつなぐ。
pub trait Connector: Send + 'static {
    type Transport: ConnectTo<Client> + Send + 'static;
    /// 接続を 1 本作る。`restart` のたびに呼ばれる。
    fn connect(&mut self) -> Result<Self::Transport, String>;
    /// 今の接続を畳む（process を止める）。
    fn shutdown(&mut self) {}
    /// engine の stderr の末尾（終了の理由の手がかり）。
    fn stderr_tail(&self) -> String {
        String::new()
    }
}

/// process を起動して stdio をつなぐ。
pub struct ProcessConnector {
    launch: LaunchContext,
    child: Option<Child>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl ProcessConnector {
    pub fn new(launch: LaunchContext) -> Self {
        Self {
            launch,
            child: None,
            stderr: Arc::default(),
        }
    }
}

impl Connector for ProcessConnector {
    type Transport = ByteStreams<Compat<ChildStdin>, Compat<ChildStdout>>;

    fn connect(&mut self) -> Result<Self::Transport, String> {
        let mut command = tokio::process::Command::new(&self.launch.program);
        command
            .args(&self.launch.args)
            .current_dir(&self.launch.cwd)
            .envs(self.launch.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in &self.launch.env_remove {
            command.env_remove(name);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("{}: {e}", self.launch.program.display()))?;
        let stdin = child.stdin.take().ok_or("stdin を取れません")?;
        let stdout = child.stdout.take().ok_or("stdout を取れません")?;
        if let Some(mut stderr) = child.stderr.take() {
            let tail = Arc::clone(&self.stderr);
            tail.lock().expect("stderr lock").clear();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = stderr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut tail = tail.lock().expect("stderr lock");
                    tail.extend_from_slice(&buf[..n]);
                    let over = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                    tail.drain(..over);
                }
            });
        }
        self.child = Some(child);
        Ok(ByteStreams::new(stdin.compat_write(), stdout.compat()))
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            // 終了を回収する。runtime の外（drop）なら kill_on_drop に任せる。
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
    }

    fn stderr_tail(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().expect("stderr lock"))
            .trim()
            .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// 起動から会話の確定まで。
    Starting,
    Ready,
    Failed(HostError),
}

/// 外から読める状態。
struct Shared {
    session_id: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    commands: Vec<String>,
    command_docs: HashMap<String, String>,
    config: Option<Vec<AcpConfigOption>>,
    config_error: Option<String>,
    /// `(使用中の token, window)`。
    usage: Option<(u64, u64)>,
    history: History,
    in_turn: bool,
    session_init: Option<ConversationEvent>,
}

struct Ctx {
    cwd: PathBuf,
    options: HostOptions,
    events: broadcast::Sender<ConversationEvent>,
    phase: watch::Sender<Phase>,
    shared: Mutex<Shared>,
}

impl Ctx {
    fn emit(&self, event: ConversationEvent) {
        // 受け手がいない時の Err は捨てる（購読は任意）。
        let _ = self.events.send(event);
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().expect("acp shared lock")
    }

    /// 今の状態から `SessionInit` を組んで覚える。sessionId が無ければ None。
    fn build_session_init(&self) -> Option<ConversationEvent> {
        let mut shared = self.shared();
        let event = ConversationEvent::SessionInit {
            session_id: shared.session_id.clone()?,
            model: shared.model.clone(),
            permission_mode: shared.mode.clone(),
            cwd: Some(self.cwd.display().to_string()),
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            slash_commands: shared.commands.clone(),
            command_docs: shared.command_docs.clone(),
        };
        shared.session_init = Some(event.clone());
        Some(event)
    }

    fn snapshot(&self) -> HistorySnapshot {
        let shared = self.shared();
        shared.history.snapshot(shared.in_turn)
    }
}

type Reply<T> = oneshot::Sender<Result<T, HostError>>;

enum Command {
    Submit {
        text: String,
        images: Vec<PromptImage>,
    },
    Interrupt,
    SetConfig {
        id: String,
        value: String,
        reply: Reply<Vec<AcpConfigOption>>,
    },
    Restart {
        reply: Reply<HistorySnapshot>,
    },
    Stop,
}

/// 接続 1 本の終わり方。
enum Exit {
    Stop,
    Restart(Reply<HistorySnapshot>),
    /// 会話が確定しなかった。
    Establish(String),
    /// 確定の後に engine が終了した / 接続が切れた。
    Dead(String),
}

/// ACP engine の host。
pub struct AcpClientHost {
    ctx: Arc<Ctx>,
    commands: mpsc::UnboundedSender<Command>,
}

impl AcpClientHost {
    /// process を起動し、initialize → `session/load`（resume あり）か `session/new` を始める。
    pub fn spawn(launch: LaunchContext, options: HostOptions) -> Result<Self, HostError> {
        let cwd = launch.cwd.clone();
        Self::spawn_with(ProcessConnector::new(launch), cwd, options)
    }

    /// 接続の作り方を差し替えて起動する。
    pub fn spawn_with<C: Connector>(
        mut connector: C,
        cwd: PathBuf,
        options: HostOptions,
    ) -> Result<Self, HostError> {
        let transport = connector.connect().map_err(HostError::Spawn)?;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (phase, _) = watch::channel(Phase::Starting);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(Ctx {
            cwd,
            events,
            phase,
            shared: Mutex::new(Shared {
                session_id: None,
                model: None,
                mode: None,
                commands: Vec::new(),
                command_docs: HashMap::new(),
                config: None,
                config_error: None,
                usage: None,
                history: History::new(options.history_limits),
                in_turn: false,
                session_init: None,
            }),
            options,
        });
        tokio::spawn(supervise(
            connector,
            transport,
            Arc::clone(&ctx),
            command_rx,
        ));
        Ok(Self { ctx, commands })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent> {
        self.ctx.events.subscribe()
    }

    /// 発話を送る。turn の最中なら、終わってから順に送る。
    pub fn submit(&self, text: &str, images: &[PromptImage]) -> Result<(), HostError> {
        self.send(Command::Submit {
            text: text.to_string(),
            images: images.to_vec(),
        })
    }

    /// 今の turn を中断する（`session/cancel`）。turn の最中でなければ何もしない。
    pub fn interrupt(&self) -> Result<(), HostError> {
        self.send(Command::Interrupt)
    }

    /// 設定の 1 軸を切り替える。成功したら engine が返した設定面の全体。
    pub async fn set_config(
        &self,
        id: &str,
        value: &str,
    ) -> Result<Vec<AcpConfigOption>, HostError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::SetConfig {
            id: id.to_string(),
            value: value.to_string(),
            reply,
        })?;
        match tokio::time::timeout(CONFIG_SET_TIMEOUT, rx).await {
            Ok(result) => result.map_err(|_| self.dead())?,
            Err(_) => Err(HostError::Engine(
                "設定の切替の応答がありません".to_string(),
            )),
        }
    }

    pub fn config_view(&self) -> Option<AcpConfigView> {
        let shared = self.ctx.shared();
        Some(AcpConfigView {
            options: shared.config.clone()?,
            error: shared.config_error.clone(),
        })
    }

    /// 最後に出した `SessionInit`（開き直しで配り直す用）。
    pub fn session_init(&self) -> Option<ConversationEvent> {
        self.ctx.shared().session_init.clone()
    }

    /// 履歴の控え。会話の確定まで待つ。呼び手が `ReplayStart` 〜 `ReplayEnd` で包む。
    pub async fn history(&self) -> Result<HistorySnapshot, HostError> {
        let mut phase = self.ctx.phase.subscribe();
        let settled = phase
            .wait_for(|p| !matches!(p, Phase::Starting))
            .await
            .map_err(|_| self.dead())?
            .clone();
        match settled {
            Phase::Failed(error) => Err(error),
            _ => Ok(self.ctx.snapshot()),
        }
    }

    /// process を立て直して load し直し、控えを置き換える。
    ///
    /// 他の書き手（console 等）が会話を進めた後、次の発話の前に呼ぶ。生きている engine は
    /// 他の書き手の分を知らず、そのまま発話を送ると保存からも消える（agy 1.5.11 の実測）。
    /// turn の最中は [`HostError::Busy`]。
    pub async fn restart(&self) -> Result<HistorySnapshot, HostError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Restart { reply })?;
        rx.await.map_err(|_| self.dead())?
    }

    /// engine を止める。
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    fn send(&self, command: Command) -> Result<(), HostError> {
        self.commands.send(command).map_err(|_| self.dead())
    }

    fn dead(&self) -> HostError {
        match &*self.ctx.phase.borrow() {
            Phase::Failed(error) => error.clone(),
            _ => HostError::Dead("host の task が終了しました".to_string()),
        }
    }
}

impl Drop for AcpClientHost {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 接続を回し、`restart` なら process を立て直して続ける。
async fn supervise<C: Connector>(
    mut connector: C,
    first: C::Transport,
    ctx: Arc<Ctx>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut resume = ctx.options.resume.clone();
    let mut transport = Some(first);
    let mut restarting: Option<Reply<HistorySnapshot>> = None;
    loop {
        let exit = match transport.take().map_or_else(|| connector.connect(), Ok) {
            Ok(transport) => {
                run_connection(
                    transport,
                    &ctx,
                    resume.clone(),
                    &mut commands,
                    &mut restarting,
                )
                .await
            }
            Err(message) => Exit::Establish(message),
        };
        connector.shutdown();
        let error = match exit {
            Exit::Restart(reply) => {
                let mut shared = ctx.shared();
                resume = shared.session_id.clone();
                shared.history = History::new(ctx.options.history_limits);
                shared.in_turn = false;
                drop(shared);
                ctx.phase.send_replace(Phase::Starting);
                restarting = Some(reply);
                continue;
            }
            Exit::Stop => HostError::Dead("停止しました".to_string()),
            Exit::Establish(message) => {
                HostError::Establish(with_stderr(message, &connector.stderr_tail()))
            }
            Exit::Dead(message) => HostError::Dead(with_stderr(message, &connector.stderr_tail())),
        };
        ctx.shared().in_turn = false;
        if !matches!(error, HostError::Dead(ref m) if m == "停止しました") {
            ctx.emit(ConversationEvent::EngineExited {
                message: error.to_string(),
            });
        }
        if let Some(reply) = restarting.take() {
            let _ = reply.send(Err(error.clone()));
        }
        ctx.phase.send_replace(Phase::Failed(error));
        return;
    }
}

fn with_stderr(message: String, stderr: &str) -> String {
    if stderr.is_empty() {
        message
    } else {
        format!("{message}\n{stderr}")
    }
}

/// 接続 1 本: initialize → load / new → 設定の当て直し → 命令のループ。
async fn run_connection(
    transport: impl ConnectTo<Client> + 'static,
    ctx: &Arc<Ctx>,
    resume: Option<String>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    restarting: &mut Option<Reply<HistorySnapshot>>,
) -> Exit {
    let policy = Arc::clone(&ctx.options.permission);
    let result = Client
        .v1()
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let outcome = match policy(&request) {
                    PermissionDecision::Select(id) => {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                    }
                    PermissionDecision::Cancelled => RequestPermissionOutcome::Cancelled,
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async |cx: ConnectionTo<Agent>| {
            let establish =
                tokio::time::timeout(ctx.options.establish_timeout, establish(&cx, ctx, resume))
                    .await;
            let mut conn = match establish {
                Ok(Ok(conn)) => conn,
                Ok(Err(message)) => return Ok(Exit::Establish(message)),
                Err(_) => {
                    return Ok(Exit::Establish(format!(
                        "{} 秒以内に確定しませんでした",
                        ctx.options.establish_timeout.as_secs()
                    )));
                }
            };
            ctx.phase.send_replace(Phase::Ready);
            if let Some(reply) = restarting.take() {
                let _ = reply.send(Ok(ctx.snapshot()));
            }
            Ok(conn.run(&cx, ctx, commands).await)
        })
        .await;
    result.unwrap_or_else(|e| Exit::Dead(e.to_string()))
}

/// 確定した接続の状態（命令のループが持つ）。
struct Connection {
    session: ActiveSession<'static, Agent>,
    session_id: String,
    translator: AcpTranslator,
    queue: VecDeque<(String, Vec<PromptImage>)>,
    in_turn: bool,
    /// turn の最中に slash command が変わった。turn の終わりに `SessionInit` を出し直す。
    init_deferred: bool,
}

/// 設定の切替の応答（接続の task から命令のループへ戻す）。
struct ConfigSet {
    reply: Reply<Vec<AcpConfigOption>>,
    result: Result<Vec<AcpConfigOption>, String>,
}

async fn establish(
    cx: &ConnectionTo<Agent>,
    ctx: &Arc<Ctx>,
    resume: Option<String>,
) -> Result<Connection, String> {
    let init = cx
        .send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await
        .map_err(|e| format!("initialize: {e}"))?;
    tracing::debug!(agent = ?init.agent_info, "ACP initialize");

    let mut loaded = None;
    if let Some(session_id) = resume {
        match cx
            .load_session(session_id.clone(), &ctx.cwd)
            .block_task()
            .start_session()
            .await
        {
            Ok(SessionSetup {
                session, response, ..
            }) => {
                let meta = serde_json::to_value(&response).unwrap_or_default();
                loaded = Some((session, session_id, meta, true));
            }
            Err(e) => match ctx.options.on_load_failed {
                LoadFallback::Fail => return Err(format!("session/load: {e}")),
                LoadFallback::NewSession => {
                    tracing::warn!("session/load 失敗 → 方針により新規の会話にする: {e}");
                    ctx.emit(ConversationEvent::Notice {
                        message: format!(
                            "前の会話を再開できなかったため、新しい会話を始めました: {e}"
                        ),
                    });
                }
            },
        }
    }
    let (session, session_id, meta, replay) = match loaded {
        Some(loaded) => loaded,
        None => {
            let SessionSetup {
                session, response, ..
            } = cx
                .build_session(&ctx.cwd)
                .block_task()
                .start_session()
                .await
                .map_err(|e| format!("session/new: {e}"))?;
            let session_id = session.session_id().to_string();
            let meta = serde_json::to_value(&response).unwrap_or_default();
            (session, session_id, meta, false)
        }
    };

    let mut conn = Connection {
        session,
        session_id: session_id.clone(),
        translator: AcpTranslator::new(),
        queue: VecDeque::new(),
        in_turn: false,
        init_deferred: false,
    };
    {
        let (model, mode) = extract_session_meta(&meta);
        let mut shared = ctx.shared();
        shared.session_id = Some(session_id.clone());
        shared.model = model;
        shared.mode = mode;
        shared.config = meta.get("configOptions").and_then(parse_config_options);
        shared.config_error = None;
    }
    if replay {
        // load が流した過去の会話。応答の前に届いた分は、もう stream に溜まっている。
        while let Some(message) = conn.session.read_update().now_or_never() {
            let message = message.map_err(|e| format!("session/load の流れ: {e}"))?;
            conn.on_message(ctx, message, true);
        }
    }
    if let Err(e) = (ctx.options.on_session)(&session_id) {
        ctx.emit(ConversationEvent::Error {
            message: format!("会話 ID を保存できません: {e}"),
        });
    }
    reapply_config(cx, ctx, &session_id).await;
    if let Some(init) = ctx.build_session_init() {
        ctx.emit(init);
    }
    Ok(conn)
}

/// 呼び手の意図を 1 軸ずつ当て直す。直前の応答で更新した設定面で要否を決める。
async fn reapply_config(cx: &ConnectionTo<Agent>, ctx: &Arc<Ctx>, session_id: &str) {
    for (id, value) in &ctx.options.config_intent {
        let needed = ctx
            .shared()
            .config
            .as_deref()
            .is_some_and(|options| needs_apply(options, id, value));
        if !needed {
            continue;
        }
        let request = SetSessionConfigOptionRequest::new(
            session_id.to_string(),
            SessionConfigId::new(id.as_str()),
            value.as_str(),
        );
        let result =
            tokio::time::timeout(CONFIG_SET_TIMEOUT, cx.send_request(request).block_task()).await;
        let mut shared = ctx.shared();
        match result {
            Ok(Ok(response)) => {
                let value = serde_json::to_value(&response.config_options).unwrap_or_default();
                shared.config = parse_config_options(&value);
                shared.config_error = None;
                if let Some(model) = current_model(shared.config.as_deref()) {
                    shared.model = Some(model);
                }
            }
            Ok(Err(e)) => shared.config_error = Some(format!("{id}={value}: {e}")),
            Err(_) => shared.config_error = Some(format!("{id}={value}: 応答がありません")),
        }
    }
}

fn current_model(options: Option<&[AcpConfigOption]>) -> Option<String> {
    options?
        .iter()
        .find(|o| o.id == "model")
        .map(|o| o.current_value.clone())
        .filter(|v| !v.is_empty())
}

impl Connection {
    async fn run(
        &mut self,
        cx: &ConnectionTo<Agent>,
        ctx: &Arc<Ctx>,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Exit {
        let (config_tx, mut config_rx) = mpsc::unbounded_channel::<ConfigSet>();
        loop {
            tokio::select! {
                // engine からの流れを先に読み切る（EOF より前に届いた分を落とさない）。
                biased;
                message = self.session.read_update() => match message {
                    Ok(message) => self.on_message(ctx, message, false),
                    Err(e) => return Exit::Dead(e.to_string()),
                },
                Some(done) = config_rx.recv() => {
                    let mut shared = ctx.shared();
                    match &done.result {
                        Ok(options) => {
                            shared.config = Some(options.clone());
                            shared.config_error = None;
                            if let Some(model) = current_model(Some(options)) {
                                shared.model = Some(model);
                            }
                        }
                        Err(message) => shared.config_error = Some(message.clone()),
                    }
                    drop(shared);
                    let _ = done.reply.send(done.result.map_err(HostError::Engine));
                }
                command = commands.recv() => match command {
                    None | Some(Command::Stop) => return Exit::Stop,
                    Some(Command::Submit { text, images }) => {
                        self.queue.push_back((text, images));
                        if let Err(message) = self.send_next(ctx) {
                            return Exit::Dead(message);
                        }
                    }
                    Some(Command::Interrupt) => {
                        if self.in_turn
                            && let Err(e) = cx.send_notification(CancelNotification::new(
                                self.session_id.clone(),
                            ))
                        {
                            return Exit::Dead(e.to_string());
                        }
                    }
                    Some(Command::SetConfig { id, value, reply }) => {
                        let request = SetSessionConfigOptionRequest::new(
                            self.session_id.clone(),
                            SessionConfigId::new(id.as_str()),
                            value.as_str(),
                        );
                        let config_tx = config_tx.clone();
                        let sent = cx.send_request(request).on_receiving_result(
                            async move |result| {
                                let result = result
                                    .map(|response| {
                                        let value = serde_json::to_value(&response.config_options)
                                            .unwrap_or_default();
                                        parse_config_options(&value).unwrap_or_default()
                                    })
                                    .map_err(|e| format!("{id}={value}: {e}"));
                                let _ = config_tx.send(ConfigSet { reply, result });
                                Ok(())
                            },
                        );
                        if let Err(e) = sent {
                            return Exit::Dead(e.to_string());
                        }
                    }
                    Some(Command::Restart { reply }) => {
                        if self.in_turn || !self.queue.is_empty() {
                            let _ = reply.send(Err(HostError::Busy));
                        } else {
                            return Exit::Restart(reply);
                        }
                    }
                },
                () = cx.incoming_closed() => {
                    return Exit::Dead("engine の出力が閉じました".to_string());
                }
            }
        }
    }

    /// 待っている発話があり、turn の最中でなければ、次の 1 件を送る。
    fn send_next(&mut self, ctx: &Arc<Ctx>) -> Result<(), String> {
        if self.in_turn {
            return Ok(());
        }
        let Some((text, images)) = self.queue.pop_front() else {
            return Ok(());
        };
        let mut blocks = vec![ContentBlock::Text(TextContent::new(text.clone()))];
        blocks.extend(
            images
                .into_iter()
                .map(|image| ContentBlock::Image(ImageContent::new(image.data, image.media_type))),
        );
        self.session
            .send_prompt_blocks(blocks)
            .map_err(|e| e.to_string())?;
        self.in_turn = true;
        let mut shared = ctx.shared();
        shared.in_turn = true;
        // 控えには送信の時の text を入れる。engine が返す写しには頼らない。
        shared.history.push(ConversationEvent::UserMessage { text });
        Ok(())
    }

    /// engine からの 1 件。`replay` は load が流した過去の会話（控えにだけ入れる）。
    fn on_message(&mut self, ctx: &Arc<Ctx>, message: SessionMessage, replay: bool) {
        match message {
            SessionMessage::PromptResponse(result) => self.on_turn_end(ctx, result),
            SessionMessage::Message(dispatch) => match dispatch {
                Dispatch::Notification(notification) => {
                    if notification.method() != "session/update" {
                        tracing::debug!(method = notification.method(), "知らない通知を読み飛ばす");
                        return;
                    }
                    if let Some(update) = notification.params().get("update") {
                        self.on_update(ctx, update, replay);
                    }
                }
                Dispatch::Request(request, responder) => {
                    // permission は builder の handler が先に答える。ここに来るのは未対応の要求。
                    tracing::warn!(method = request.method(), "未対応の要求を断る");
                    let _ = responder
                        .respond_with_error(agent_client_protocol::Error::method_not_found());
                }
                Dispatch::Response(..) => {}
            },
            // 将来 SDK が種類を足しても読み続ける。
            _ => {}
        }
    }

    fn on_update(&mut self, ctx: &Arc<Ctx>, update: &serde_json::Value, replay: bool) {
        if let Some((commands, docs)) = extract_available_commands(update) {
            let mut shared = ctx.shared();
            shared.commands = commands;
            shared.command_docs = docs;
            drop(shared);
            self.refresh_session_init(ctx, replay);
            return;
        }
        if let Some(usage) = extract_usage_update(update) {
            ctx.shared().usage = Some(usage);
            return;
        }
        if let Some(mode) = extract_current_mode(update) {
            ctx.shared().mode = Some(mode);
            self.refresh_session_init(ctx, replay);
            return;
        }
        if let Some(options) = crate::config::parse_config_option_update(update) {
            let mut shared = ctx.shared();
            if let Some(model) = current_model(Some(&options)) {
                shared.model = Some(model);
            }
            shared.config = Some(options);
            return;
        }
        for event in self.translator.ingest(update) {
            if replay {
                ctx.shared().history.push(event);
                continue;
            }
            // host が出した発話の写し。UI は送信の時に表示済みで、控えにも入れてある。
            if self.in_turn && matches!(event, ConversationEvent::UserMessage { .. }) {
                continue;
            }
            ctx.shared().history.push(event.clone());
            ctx.emit(event);
        }
    }

    /// `SessionInit` を出し直す。turn の最中は終わりまで保留する（途中で出すと UI の状態が動く）。
    fn refresh_session_init(&mut self, ctx: &Arc<Ctx>, replay: bool) {
        if replay {
            return; // 確定の時にまとめて出す
        }
        if self.in_turn {
            self.init_deferred = true;
        } else if let Some(init) = ctx.build_session_init() {
            ctx.emit(init);
        }
    }

    fn on_turn_end(
        &mut self,
        ctx: &Arc<Ctx>,
        result: Result<PromptResponse, agent_client_protocol::Error>,
    ) {
        if let Err(e) = result {
            ctx.emit(ConversationEvent::Error {
                message: e.to_string(),
            });
        }
        self.in_turn = false;
        let usage = {
            let mut shared = ctx.shared();
            shared.in_turn = false;
            shared.usage
        };
        ctx.emit(ConversationEvent::TurnCompleted {
            session_id: self.session_id.clone(),
            cost_usd: None,
            context_tokens: usage.map(|(used, _)| used),
            context_window: usage.map(|(_, size)| size),
        });
        if std::mem::take(&mut self.init_deferred)
            && let Some(init) = ctx.build_session_init()
        {
            ctx.emit(init);
        }
        if let Err(message) = self.send_next(ctx) {
            ctx.emit(ConversationEvent::Error { message });
        }
    }
}
