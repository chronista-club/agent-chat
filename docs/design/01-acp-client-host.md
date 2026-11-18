# 01. ACP Client host — 公式 SDK で ACP engine を共有側から動かす

> **Status**: Draft — 設計のみ。実装は未着手（2026-11-18: 履歴の復元を Stage 1 の要件に追加）
> **Related**: spark `mem_1Ch7ppZtnWBXnFjgLa7v2G`（mako 2026-11-17）、task `mem_1Ch7tUUcx4iNoDifCNG9gd`、VP design 86（共有チャット基盤）、VP design 88（agy の ACP 統合）
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
  - **会話の履歴の正を engine にする**。`session/load` が流す過去の会話を捨てずに、履歴として呼び手に渡す（「履歴」の節）
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
| event の配送先 | broadcast の受け手 | 会話中の `ConversationEvent` を送る。load が流す過去の会話は broadcast に流さず、履歴の控えに入れる |
| 履歴 | 履歴を取りに行く時機（chat を開いた時）、他の書き手（console）が進めたかの判断 | 控えの保持、`history()` での受け渡し、`reload_history()` での取り直し（「履歴」の節） |
| 設定面 | — | configOptions の保持と view、`session/set_config_option`、spawn 後の当て直し |

VP に残すもの:
- `EngineKind` / `AcpEngine` の表（どの CLI をどう起動するか）
- registry と `SessionKey`、identity env（`VP_REPO` など。起動の文脈として渡す）
- roster への投影
- 履歴を `ReplayStart` 〜 `ReplayEnd` で包んで配る合流の処理（`conversation_replay`）。ACP engine では、出どころを replay_log からこの host の `history()` に替える（Stage 2）
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
- load が流す過去の会話は、応答の前に届いた分が同じ stream に溜まっている。`start_session()` が返った時点で溜まっている分を読み切り、履歴の控えにする
- prompt の結果（`PromptResponse`）は、その turn の update の後に同じ stream で来る。turn の終わりの判定が 1 本の順序にまとまる
- builder に型付きの `SessionNotification` handler を置くと、session の handler より先に通知を食べてしまう（`incoming_actor.rs` の登録順）。なので置かない

知らない種類の通知:
- `ActiveSession` は通知を sessionId だけで振り分け、中身は生のまま渡す
- host が自分で parse し、知らない `sessionUpdate` は log を出して捨て、続ける（VP と同じ寛容さ）

### 履歴 — `session/load` の流れを捨てずに渡す

経緯（2026-11-17 に mako が実機で確認）:
- chat → console は続きができた
- console → chat は、engine は覚えているのに画面に履歴が出なかった
- 原因: VP の chat は replay_log（VP が chat の event を記録した控え）からしか戻さず、`session/load` が流す過去の会話を捨てている。console で進めた分は replay_log に無い

直す場所は VP の手書きの host ではなく、この crate にする（VP と GFP の両方が使う）。

#### 決めたこと

1. **履歴は broadcast に流さない。host が控えを持ち、`history()` で渡す**
   - 控え = load の流れを訳した event 列 + その後の会話中の event
   - 呼び手は `history()` の結果を `ReplayStart` 〜 `ReplayEnd { in_flight }` で包んで配る。VP なら `conversation_replay` の `replay_log::load` を差し替える位置で、区切りの置き方は今のまま
   - broadcast に流さない理由: load は spawn の直後に走る。broadcast は subscribe の前の event を落とすので、受け手が間に合う保証が無い
   - `history()` は会話の確定（load か new の応答）まで待ってから返す。失敗したら Err
   - Codex の「host が履歴を一括で渡す」（`CodexHistory`）と同じ向き
2. **engine が生きたまま chat を開き直す時は、控えから出す**（その時は `session/load` が走らない）
   - 開き直しと最初の表示が同じ口（`history()`）になる。turn の最中でも出せる（`in_flight: true`）
   - 呼び手が控えを持つ形（今の replay_log）にはしない。VP と GFP がそれぞれ作り直すことになり、console の分が抜ける穴も残る
3. **他の書き手（console）が進めた分は、`reload_history()` で取り直す**
   - 生きている接続に `session/load` をもう一度送り、流れで控えを置き換える
   - 実測（agy 1.5.11）: 同じ process に `session/load` を 3 回送ると、3 回とも同じ流れが 0.11 秒で返った
   - turn の最中は送らない（Err を返す。控えはそのまま）
   - 取り直しの後は、設定の当て直しをもう一度行う（agy は load の後に既定へ戻る）
   - いつ呼ぶかは呼び手が決める。VP は「console が動いた後に chat を開いた」を知っている
4. **控えには上限を置く**（`CodexHistory` と同じ考え方: 全体の byte 数、event の数、1 件の大きさ）
   - 超えたら古い方から落とし、`truncated: true` で知らせる
   - 実測で 3.4 MB の会話がある（大半は tool の結果）。1 件の大きさで切ると大きく減る見込みだが、値は実装で測って決める

#### 流れの訳し方

実測（agy 1.5.11、「実測の記録」の節）:
- 過去の通知は load の応答の**前**に、起きた順で流れる
- turn の区切りの通知は無い。`tool_call_update` も出ない（`tool_call` 1 件に status と結果まで入っている）
- 応答の**後**に usage_update / available_commands_update / session_info_update が 1 件ずつ来る

訳し方:
- **turn の区切り**: `user_message_chunk` が来たら新しい turn が始まったとみなす。履歴には `TurnCompleted` を入れない（VP の replay と同じ。終わりは `ReplayEnd` が下ろす）
- **user の発話**: `user_message_chunk` を `UserMessage` にする。連続して来た分は 1 つの発話に連結する（agy の実測は 1 発話 1 chunk。messageId は無い）
- **tool**: `tool_call` の status が completed / failed なら、`ToolCall` と `ToolCallUpdate` の 2 つにする。会話中も同じ規則
- **履歴に入れないもの**: usage / commands / 設定 / session_info は状態として持つだけ（今と同じ）

#### 会話中の `user_message_chunk`

agy は会話中も、送った発話を `user_message_chunk` で返してくる（VP design 88 §1）。

- **host が出した prompt の turn の中**: broadcast に出さない。UI は送信の時に自分で発話を表示しているので、二重になる
- **控えには、送信の時に text を `UserMessage` として入れる**。返ってくる chunk には頼らない（返さない engine があっても控えが欠けない）
- **host が prompt を出していないのに来た時**: `UserMessage` として broadcast にも出す（他の書き手の発話。stdio の 1 接続では起きないはずだが、捨てない）

#### まだ決められないこと（実測してから書く）

- **生きている `agy acp` は、console が進めた分を知っているか**: `reload_history()` で表示は新しくなる。engine の側の文脈が古いままなら、表示と食い違う。その場合は「他の書き手が進めたら process を立て直す」を正にし、`reload_history()` は使わない
- **grok / opencode の load の流れ**、同じ process での 2 回目の load を受けるか
- **画像つきの発話**が流れでどう見えるか（`UserMessage` は text だけ）
- **中断した turn** の見え方
- **SDK**: 同じ session id で `load_session` をもう一度呼べるか（前の `ActiveSession` を落としてから呼ぶ想定。偽の agent で確かめる）

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
    /// 履歴の控え。会話の確定まで待つ。呼び手が ReplayStart 〜 ReplayEnd で包む
    pub async fn history(&self) -> Result<HistorySnapshot>;
    /// engine から履歴を取り直す（session/load をもう一度）。turn の最中は Err
    pub async fn reload_history(&self) -> Result<HistorySnapshot>;
    pub fn stop(&mut self);
}

pub struct HistorySnapshot {
    pub events: Vec<ConversationEvent>,
    /// 今 turn の最中か（ReplayEnd にそのまま載せる）
    pub in_flight: bool,
    /// 上限を超えて古い方を落としたか
    pub truncated: bool,
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
- 履歴の控え（event 列、大きさ、切り詰めたか）
- 命令の queue（text と画像）、当て直しの残り、turn 中か、`SessionInit` の保留
- stderr の末尾、dead

命令のループは 1 つの task が持つ。`ActiveSession::read_update` は `&mut self` なので、外から直接は触らせない。外の呼び手とは mpsc でつなぐ。

### 翻訳の表（v1 `SessionUpdate` → `ConversationEvent`）

| SessionUpdate | 出すもの |
|---|---|
| `agent_message_chunk` | `MessageChunk` |
| `agent_thought_chunk` | `ThoughtChunk` |
| `tool_call` | `ToolCall`。status が completed / failed なら `ToolCallUpdate` も続ける（load の流れはこの形で来る） |
| `tool_call_update` | `ToolCallUpdate`（content の text と diff）。開始を見ていなければ `ToolCall` を補う |
| `plan` | `Plan` |
| `available_commands_update` | `SessionInit` を出し直す。turn 中は終わりまで保留（VP design 88 §6.3） |
| `usage_update` | 状態に持ち、`TurnCompleted` に載せる |
| `config_option_update` / `current_mode_update` | 状態に持つ（設定面の view） |
| `user_message_chunk` | load の流れ: `UserMessage`（連続分は連結）。会話中: host が出した prompt の turn の中なら出さない、そうでなければ `UserMessage` |
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
   - 履歴: 「load の流れ → `history()` の event 列」「turn の後の `history()`」「turn の最中の `history()`（`in_flight`）」「`reload_history()` で控えが置き換わる」「上限を超えたら `truncated`」
4. 実機 E2E（`#[ignore]`）: agy 1.5.9。model は `gemini-3.8-flash`（既定の Gemini 4 Argon は使用枠が切れていることがある）
5. 比較: 同じシナリオ（新規 → text → 画像 → 設定の切替 → load → 当て直し → cancel）で、VP の `AcpAgentHost` と event の並びを並べ、差を記録する

### 実機で測る項目

1. **initialize の protocolVersion**: SDK は応答が 1 ちょうどであることを求める（`src/jsonrpc/protocol_compat.rs`）。agy は 1 を返した（VP の実測）が、SDK 経由で通るかを見る
2. **通知の種類と field が v1 の型で読めるか**: 読めない通知は SDK が warn で捨てる。`Lines` adapter で生の行も記録して突き合わせる
3. **prompt の応答の後に update が来ないか**: 来ると次の turn に混ざる。VP の実測で 1 度、本文が空のまま turn が終わった件（VP design 88 §6.4）の確認も兼ねる
4. **load の replay**: 応答の前に届いた分が、`start_session()` が返った時点で stream に溜まっていること。量
5. **cancel**: `session/cancel` と、応答待ちの permission の要求の順序（Cancelled で答える）。request の future を落とすと SDK が `$/cancel_request` を送るので、使い方で避ける
6. **stdout の中身**: JSON でない行、UTF-8 でない byte が出ないか。UTF-8 でないと接続が切れる（`src/jsonrpc/transport_actor.rs`）
7. **遅い handler**: dispatch・transport・`cx.spawn` の task は 1 本の future の中で回るので、遅い handler は接続全体を止める。permission の判断の関数で重い処理をしない
8. **履歴**（「まだ決められないこと」の 5 つ）: 生きている process が console の分を知っているか / grok と opencode の load / 画像つきの発話 / 中断した turn / SDK での 2 回目の load

### 実測の記録

agy 1.5.11、2026-11-18。`agy acp` に initialize → session/list → session/load を直接送った。本文は見ていない（種類・key・件数・時間だけ）。

load の流れ（VP lead の実測）:

| 会話 | user_message_chunk | tool_call | agent_thought_chunk | agent_message_chunk | 量 | 応答まで |
|---|---|---|---|---|---|---|
| a517c899… | 1 | 5 | 2 | 1 | 6 KB | 0.11 秒 |
| 2b76f5cf… | 1 | 11 | 3 | 2 | 54 KB | 0.11 秒 |
| 4924fbc2…（console と行き来した会話） | 12 | 147 | 63 | 16 | 3.4 MB | 0.17 秒 |

形（4924fbc2… で確認）:
- chunk の key は `content` と `sessionUpdate` だけ。content は全て `type: text`。messageId は無い
- `tool_call` の key: args, content, kind, locations, name, rawInput, rawOutput, status, title, toolCallId（content と locations は無いことがある）
- `tool_call` の status: completed 145、failed 2。途中の状態は無い
- `tool_call` の content の block: text 76、diff 20、resource_link 1
- 並び: user の発話 1 件の後に、thought と tool が交互に続き、agent の発話で終わる。agent の発話が無いまま次の user の発話が来る所が 1 つある

同じ process での 2 回目の load（a517c899…）:
- `session/load` を 3 回続けて送ると、3 回とも成功し、同じ流れ（同じ種類・同じ件数・同じ byte 数）が返った。応答まで 0.13 / 0.11 / 0.11 秒
- 3 回とも、応答の後に usage_update / available_commands_update / session_info_update が 1 件ずつ来た
- initialize の応答: protocolVersion 1、`loadSession: true`、sessionCapabilities に resume / close / load / list

### リスク

- **SDK の版の追従**: 5.x で、schema は `=` で固定され、minor の版でも動く。unstable の feature は使わない。版は workspace の 1 か所で持つ
- **境界**: process を共有側が持つ最初の host になる。load の失敗の扱いが、VP の今（黙って新規）と design 86（変換しない）で食い違う。方針を注入にして既定は `Fail`、VP は `NewSession` を渡す
- **GFP**: crate を分けるので、GFP が依存するまで build に影響しない
- **Windows**: tokio::process と compat は Windows でも動く想定。未確認

### Stage 2 で決めること

- VP の切替スイッチの形（env か settings か）。最初は agy だけ
- submodule の revision を動かすので、GFP と同じ revision に揃える（design 86 の gate）
- VP の `AcpAgentHost` を残す期間（比較のために並べて置く → 置き換える）
