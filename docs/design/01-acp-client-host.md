# 01. ACP Client host — 公式 SDK で ACP engine を共有側から動かす

> **Status**: Draft — 設計のみ。実装は未着手
> **Related**: spark `mem_1Ch7ppZtnWBXnFjgLa7v2G`（mako 2026-11-17）、VP design 86（共有チャット基盤）、VP design 88（agy の ACP 統合）
> **対象**: `crates/acp/`（新しい crate `agent-chat-acp`、未作成）

## Abstract

agent-chat に、ACP（Agent Client Protocol、protocol v1）の **client host** を置く。
ACP の接続・型・順序の管理は、公式の Rust SDK `agent-client-protocol` 5.3.0 に任せる。

- **今**: VP は ACP engine（grok / opencode / agy）を `AcpAgentHost` で動かしている。
  - JSON-RPC を手で組み、`session/update` を自前で解釈している
  - schema の追従（config options、usage、新しい通知の種類）も自前で持っている
- **狙い**:
  - SDK の型と接続管理に寄せて、手書きの部分を減らす
  - 共有側に置いて、GFP も ACP engine を使えるようにする
  - VP design 86 の「共有 host は stdio と engine protocol を所有する」に沿う、最初の lifecycle 込みの host にする

段階:

| 段階 | 中身 | VP / GFP への影響 |
|---|---|---|
| **Stage 1**（実験） | crate を作る。偽の agent で単体テスト、実機の agy で E2E。同じシナリオで VP の `AcpAgentHost` と event の並びを比べる | なし（submodule の revision は動かさない） |
| **Stage 2**（GUI 統合） | VP の agy の chat host を、切替スイッチでこの host に差し替えて GUI で試す | VP の submodule の revision が動く。GFP と同じ revision に揃える（design 86 の gate） |

本書は Stage 1 の設計と、Stage 2 で決めることを書く。

やらないこと:
- protocol v2（SDK では unstable）
- client capability の `fs` / `terminal`
- MCP over ACP
- grok / opencode の置き換え（Stage 2 も agy から）

## Architecture

### 置き場と依存

新しい crate `crates/acp`（package `agent-chat-acp`）にする。既存の `agent-chat-host` には入れない。GFP が使うと決めるまで、GFP の build に SDK の依存を持ち込まないためである。

| 依存 | 用途 |
|---|---|
| `agent-client-protocol = "5.3"`（feature なし） | 接続・型・dispatch。`process` / `stdio` は使わない — async-io の reactor と blocking の thread pool を持ち込む（SDK `src/stdio.rs`） |
| `agent-chat-protocol` | 出力の `ConversationEvent` |
| `tokio`（process / io-util / sync / macros / rt） | process の起動、命令の channel |
| `tokio-util`（feature `compat`） | tokio の pipe を futures の `AsyncRead` / `AsyncWrite` に変える（`ByteStreams` の要求） |
| `futures` / `serde_json` / `tracing` | — |

SDK は runtime に依存しない（futures の channel だけ）。接続と builder と session の future は `Send + 'static` で、SDK の `tests/auto_traits.rs` が保証している。tokio の multi-thread の上で `tokio::spawn` でき、`LocalSet` は要らない。

### 責務の分け方（design 86 の注入契約に対応）

| 契約 | アプリが渡す | 共有 host が持つ |
|---|---|---|
| 起動の文脈 | 実行ファイル、引数、cwd、足す env、剥がす env | process の起動（tokio::process、piped stdio、kill_on_drop）、stderr の末尾の保持 |
| 会話の紐付け | 再開する sessionId（無ければ新規）、load に失敗したときの方針 | initialize → `session/load` か `session/new`。load の失敗を**黙って新規に変えない**（design 86「resume 失敗を新規会話へ変換しない」）。方針が新規なら新規にし、その事実を event で出す |
| 永続化 | session 確定時の callback（VP: registry に会話 id を書く）、spawn 時の設定の意図 | callback の失敗を event で表に出す |
| permission の方針 | 判断する関数 | `session/request_permission` を受けて関数に渡し、応答を返す。既定は kind で allow_once → allow_always、無ければ Cancelled |
| event の配送先 | broadcast の受け手 | `ConversationEvent` を送る。load 中の過去の通知（replay）は流さない |
| 設定面 | — | configOptions の保持と view、`session/set_config_option`、spawn 後の当て直し |

VP に残すもの:
- `EngineKind` / `AcpEngine` の表（どの CLI をどう起動するか）
- registry と `SessionKey`、identity env（`VP_REPO` など。起動の文脈として渡す）
- replay_log の tap、roster への投影
- engine 別の意図（`AgySettings`）と設定 panel

### 部品（data / calculations / actions）

**calculations**（純関数。SDK の型 → agent-chat の型）
- `translate`: v1 `SessionUpdate` → `ConversationEvent`。VP の `acp_translate.rs` を移す（text / diff の tool 結果、開始を見ていない tool_call の補完、plan）
- `config`: v1 の config option → view の型、当てる要否（`needs_apply`）。VP の `acp_config.rs` を移す
- `permission`: `RequestPermissionRequest` から、既定の方針で option を選ぶ

**actions**
- `AcpClientHost::spawn`: process を起動し、接続の task を `tokio::spawn` する
- 接続の task:
  1. `Client.v1()` の builder に、permission の要求の handler だけを登録する
  2. `connect_with(ByteStreams::new(stdin.compat_write(), stdout.compat()), main_fn)` で繋ぐ
  3. `main_fn` の中で initialize → load か new で `ActiveSession` を得て、命令のループに入る
  4. ループは「外からの命令（mpsc）」「`session.read_update()`」「`cx.incoming_closed()`」の 3 つを select する

`ActiveSession` を使う理由（SDK `src/session.rs`、`src/concepts/sessions.rs`）:
- load の replay は、応答の前に `read_update()` から出る。load 中は捨てればよい
- prompt の結果（`PromptResponse`）は、その turn の update の後に同じ stream で来る。turn の終わりの判定が 1 本の順序にまとまる
- builder に型付きの `SessionNotification` handler を置くと、session の handler より先に通知を食べてしまう（`incoming_actor.rs` の登録順）。なので置かない

知らない種類の通知:
- `ActiveSession` は通知を sessionId だけで振り分け、中身は生のまま渡す
- host が自分で parse し、知らない `sessionUpdate` は log を出して捨て、続ける（VP と同じ寛容さ）

### 公開 API（案）

VP の `AcpAgentHost` と同じ形にして、Stage 2 を差し替えだけで済ませる。

```rust
pub struct LaunchContext {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
}

pub enum LoadFallback { NewSession, Fail }

pub struct HostOptions {
    pub resume: Option<String>,
    pub on_load_failed: LoadFallback,
    /// spawn 後に当て直す (config id, 値)。agy は load の後に既定へ戻る（VP design 88 §6.1）
    pub config_intent: Vec<(String, String)>,
    pub permission: Arc<dyn Fn(&RequestPermissionRequest) -> PermissionDecision + Send + Sync>,
    pub on_session: Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>,
}

impl AcpClientHost {
    pub fn spawn(launch: LaunchContext, options: HostOptions) -> Result<Self>;
    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent>;
    /// 画像は {media_type, data(base64)}（VP の ImageInput / codex_input と同じ形）
    pub async fn submit(&self, text: &str, images: &[PromptImage]) -> Result<()>;
    pub async fn interrupt(&self) -> Result<()>;
    pub async fn set_config(&self, id: &str, value: &str) -> Result<Vec<AcpConfigOption>, String>;
    pub fn config_view(&self) -> Option<AcpConfigView>;
    pub fn session_init(&self) -> Option<ConversationEvent>;
    pub fn stop(&mut self);
}
```

## Data Model

### host の状態

```
起動 → initialize → load（resume あり）/ new（なし）
     → 確定（sessionId）→ 設定の当て直し（1 軸ずつ）
     → 待機 ⇄ turn 中
     → 終了（stdout の EOF / stop / 接続の失敗）
```

持つ値:
- session id、model / mode（`SessionInit` に載せる）、slash command
- configOptions とその失敗の文、context の使用量
- 命令の queue（text と画像）、当て直しの残り、turn 中か、`SessionInit` の保留
- stderr の末尾、dead

命令のループは 1 つの task が持つ。`ActiveSession::read_update` は `&mut self` なので、外から直接は触らせない。外の呼び手とは mpsc でつなぐ。

### 翻訳の表（v1 `SessionUpdate` → `ConversationEvent`）

| SessionUpdate | 出すもの |
|---|---|
| `agent_message_chunk` | `MessageChunk` |
| `agent_thought_chunk` | `ThoughtChunk` |
| `tool_call` | `ToolCall` |
| `tool_call_update` | `ToolCallUpdate`（content の text と diff）。開始を見ていなければ `ToolCall` を補う |
| `plan` | `Plan` |
| `available_commands_update` | `SessionInit` を出し直す。turn 中は終わりまで保留（VP design 88 §6.3） |
| `usage_update` | 状態に持ち、`TurnCompleted` に載せる |
| `config_option_update` / `current_mode_update` | 状態に持つ（設定面の view） |
| `user_message_chunk` | 無視（UI の楽観的な表示と二重にしない） |
| `session_info_update` | 無視（title は未対応） |
| `notice` / `compaction_*` | 無視。client capability を宣言しないので来ない（agent は宣言した client にだけ送る） |
| 知らない種類 | log を出して無視 |

prompt の応答は `TurnCompleted` にする（context の使用量を載せる）。エラーなら `Error` を出してから `TurnCompleted`。

## Implementation

### Stage 1 の手順（TDD）

1. crate を足す。agent-chat の gate（fmt / clippy `-D warnings` / `test --all-features` / 生成物の drift）に乗せる
2. calculations: 翻訳・設定・permission の純関数。fixture は VP design 88 §1・§6 の実測 wire（agy 1.5.9）を移す
3. 偽の agent で host を確かめる:
   - SDK の `Agent.v1()` で agent をメモリの中に立て、`Channel::duplex()`（両端を返す汎用の adapter、SDK `src/jsonrpc.rs`）でつなぐ
   - 実 CLI 無しで、load の replay・turn・cancel・設定の切替・知らない通知・EOF を決まった形で再現できる
4. 実機 E2E（`#[ignore]`）: agy 1.5.9。model は `gemini-3.8-flash`（既定の Gemini 4 Argon は使用枠が切れていることがある）
5. 比較: 同じシナリオ（新規 → text → 画像 → 設定の切替 → load → 当て直し → cancel）で、VP の `AcpAgentHost` と event の並びを並べ、差を記録する

### 実機で測る項目

1. **initialize の protocolVersion**: SDK は応答が 1 ちょうどであることを求める（`src/jsonrpc/protocol_compat.rs`）。agy は 1 を返した（VP の実測）が、SDK 経由で通るかを見る
2. **通知の種類と field が v1 の型で読めるか**: 読めない通知は SDK が warn で捨てる。`Lines` adapter で生の行も記録して突き合わせる
3. **prompt の応答の後に update が来ないか**: 来ると次の turn に混ざる。VP の実測で 1 度、本文が空のまま turn が終わった件（VP design 88 §6.4）の確認も兼ねる
4. **load の replay**: 応答の前に `read_update` から出ること。量
5. **cancel**: `session/cancel` と、応答待ちの permission の要求の順序（Cancelled で答える）。request の future を落とすと SDK が `$/cancel_request` を送るので、使い方で避ける
6. **stdout の中身**: JSON でない行、UTF-8 でない byte が出ないか。UTF-8 でないと接続が切れる（`src/jsonrpc/transport_actor.rs`）
7. **遅い handler**: dispatch・transport・`cx.spawn` の task は 1 本の future の中で回るので、遅い handler は接続全体を止める。permission の判断の関数で重い処理をしない

### リスク

- **SDK の版の追従**: 5.x で、schema は `=` で固定され、minor の版でも動く。unstable の feature は使わない。版は workspace の 1 か所で持つ
- **境界**: process を共有側が持つ最初の host になる。load の失敗の扱いが、VP の今（黙って新規）と design 86（変換しない）で食い違う。方針を注入にして既定は `Fail`、VP は `NewSession` を渡す
- **GFP**: crate を分けるので、GFP が依存するまで build に影響しない
- **Windows**: tokio::process と compat は Windows でも動く想定。未確認

### Stage 2 で決めること

- VP の切替スイッチの形（env か settings か）。最初は agy だけ
- submodule の revision を動かすので、GFP と同じ revision に揃える（design 86 の gate）
- VP の `AcpAgentHost` を残す期間（比較のために並べて置く → 置き換える）
