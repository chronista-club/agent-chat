# 01. ACP Client host — 公式 SDK で ACP engine を共有側から動かす

> **Status**: Stage 1 を実装済み（2026-11-18、main に merge 済み）。crate、偽の agent のテスト、実機 agy の E2E、VP の手書き host との比較まで。Stage 2（VP への組み込み）は VP の側で進行中（VP PR #1271、2026-11-20 時点で draft）。その中で見つかった共有 host の直しは、この crate に入れている
> **Related**: spark `mem_1Ch7ppZtnWBXnFjgLa7v2G`（mako 2026-11-17）、task `mem_1Ch7tUUcx4iNoDifCNG9gd`、VP design 86（共有チャット基盤）、VP design 88（agy の ACP 統合）
> **対象**: `crates/acp/`（crate `agent-chat-acp`）

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
| 履歴 | 履歴を取りに行く時機（chat を開いた時）、他の書き手（console）が進めたことの通知 | 控えの保持、`history()` での受け渡し、`restart()` での process の立て直し（「履歴」の節） |
| 設定面 | — | configOptions の保持と view、`session/set_config_option`、spawn 後の当て直し |

VP に残すもの:
- `EngineKind` / `AcpEngine` の表（どの CLI をどう起動するか）
- registry と `SessionKey`、identity env（`VP_REPO` など。起動の文脈として渡す）
- roster への投影
- 履歴を `ReplayStart` 〜 `ReplayEnd` で包んで配る合流の処理（`conversation_replay`）。ACP engine では、出どころを replay_log からこの host の `history()` に替える（Stage 2）
- engine 別の意図（`AgySettings`）と設定 panel

### 部品（data / calculations / actions）

**calculations**（純関数）
- `translate`: `session/update` の update → `ConversationEvent`。VP の `acp_translate.rs` を移した（text / diff の tool 結果、開始を見ていない tool_call の補完、plan）
- `config`: config option → view の型、当てる要否（`needs_apply`）。VP の `acp_config.rs` を移した
- `session_meta`: model / mode / slash command / context の使用量の抽出。VP の `acp_host.rs` の純関数を移した
- `permission`: `RequestPermissionRequest` から、既定の方針で option を選ぶ
- `history`: 履歴の控え（turn の区切り、chunk のまとめ、上限）

`translate`・`config`・`session_meta` の入力は、SDK の型ではなく生の JSON にした。engine は標準に無い key（agy の `name` / `args`）や知らない種類（x.ai の拡張）を出す。型で読むと落ちるか捨てられるので、生のまま読んで知らないものを読み飛ばす。要求と応答（initialize / new / load / prompt / set_config_option / permission）は SDK の型を使う。

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
3. **他の書き手（console）が会話を進めたら、次の prompt の前に process を立て直す**（`restart()`）
   - 実測（agy 1.5.11、「2 人の書き手」の記録）で、生きている `agy acp` は別 process が進めた分を知らない:
     - 生きた接続に `session/load` をもう一度送っても、別 process の分は流れてこない（自分が覚えている分だけを返す）
     - そのまま prompt を送ると、別 process の分を知らずに答え、**保存からも別 process の分が消える**
     - `session/list` の updatedAt も変わらない。host は自分では気づけない
   - なので、進めたことは呼び手が知らせる。VP は「console が動いた」を知っている
   - VP で実際にこの場面が起きるか: 同じ session を chat → console に切り替えると、reconcile が chat の engine を畳む（VP nightly `504859ba` の `repo/lane/reconcile.rs`、`drop_chat_engine_by_key`）。畳むのは console の slot を立てた後。chat に戻ると engine が立ち直って load するので、console の分が出る作りになっている。この文書は 2026-11-18 から 11-20 まで「VP は切り替えても chat の engine を止めない」と書いていたが、誤りだった（呼び出し元の grep が 1 つ足りなかった）。engine の性質の実測（下の「2 人の書き手」）は正しいまま。実機では確かめていない
   - `restart()` の中身: child を止める → 同じ起動の文脈で起動 → initialize → 今の session id で `session/load` → 設定の当て直し → 控えを置き換える。subscribe はそのまま使える
   - turn の最中は Err を返す（呼び手が中断してから呼ぶ）
   - 生きた接続に `session/load` を送り直すだけの口は作らない（上の実測で、役に立たないと分かった）
4. **控えには上限を置く**（`CodexHistory` と同じ考え方: 全体の byte 数、event の数、1 件の大きさ）
   - load の流れも、その後の会話中の event も、同じ 1 つの控えに同じ上限で入れる。超えたら古い turn から落とし、`truncated: true` で知らせる
   - だから「開き直し」と「立て直し」で見える履歴の長さは変わらない（どちらも上限まで）
   - 実測で 3.4 MB / 12 turn の会話がある（大半は tool の結果）。1 件の大きさで切ると大きく減る見込みだが、値は実装で測って決める
5. **待つ上限と失敗の返り**
   - `history()` が会話の確定を待つのは、起動からの上限まで（`HostOptions::establish_timeout`、既定 30 秒。実測の確定は 2 秒ほど）。超えたら Err
   - 確定は initialize と load / new まで。**設定の当て直しは確定の上限に数えない**。当て直しは全体で `config_timeout`（既定 15 秒）まで待ち、応答が無ければ残りの軸を送らずに打ち切る。失敗しても打ち切っても host は止めず、設定面に失敗の文を残して発話を流す（VP の手書きの host と同じ）。発話は当て直しが済むか打ち切られてから送る
   - load が失敗した時は、注入された方針に従う。`Fail`（既定）なら `history()` は engine の文を載せた Err を返し、host は終了する。`NewSession` なら新規の会話になり、`history()` は空を返す。新規に変えた事実は event で出す
   - `restart()` の失敗も同じ扱い

#### 呼び手の規則

- **他の書き手（console 等）が会話を進めたら、次の発話の前に `restart()` を呼ぶ**。呼ばずに発話を送ると、他の書き手の分が保存から消える
- **`restart()` が `Busy` なら**、turn を中断するか終わりを待ってから呼び直す
- **`restart()` が `Establish` / `Dead` を返したら、host は終了している**（`LoadFallback::Fail` で load に失敗した時、起動できなかった時など）。`EngineExited` も出る。その host はもう使えないので、呼び手が作り直す（`spawn` し直す）。`history()` / `submit()` も同じ Err を返す
- **chat を開く時は `attach()`**。`history()` と `subscribe()` を別々に呼ばない
- **`is_finished()` が true の host は使わない**。chat を開く時に見て、終了していたら作り直す
  - `is_finished()` が true になってから `Error` / `EngineExited` が出るまで、最大 1 秒ある（stderr の読み切り）。その間に host を捨てて作り直すと、古い host の受け口を読むのをやめた時点で、「待っていた発話 N 件は送られていません」などの文は届かない。文を画面に出したい呼び手は、`EngineExited` を読んでから捨てる
- **`TurnCompleted` を受けても、host が終了していることがある**（中断に答えない engine を待ち切りで止めた時）。turn の終わりを見て次の発話を送る呼び手は、`submit()` の Err か `is_finished()` を見て作り直す。続けて `EngineExited` が出る
- **log の target は `agent_chat_acp`**。host の警告（load に失敗して新規に替えた、未対応の要求を断った、など）は `tracing` でこの target に出る。consumer の log の filter にこの target が無いと、警告が log から消える（VP は filter に足した）
- **`permission` と `on_session` に重い処理を置かない**（接続全体が止まる）。`on_stderr` も同じく軽くする（こちらは stderr を読む task の中で呼ばれる）

#### load の流れの読み切り

host は、`start_session()` が返った時点で stream に溜まっている分を「load が流した過去の会話」として読み切る（`read_update().now_or_never()` が None を返すまで）。「応答の前に届いた通知は、その時点で全部溜まっている」ことは、SDK の次の作りによる:

- 接続ごとに dispatch loop が 1 本あり、届いた message を 1 件ずつ、届いた順に処理する（SDK `src/concepts/ordering.rs`「The Dispatch Loop」）。通知も応答も同じ loop を通る
- `start_session()` は、session の振り分けを登録してから `session/load` を送る（`src/session.rs` の `start_restored_session`、`send_ordered_request_to_after`）。応答の前の通知は、loop が順に session の channel に入れる
- 応答は `block_task_with_ordered_result` で受ける。応答の処理が済むまで loop は次へ進まない（`src/jsonrpc.rs`、「before releasing the ordered-response barrier」）。応答が処理される時には、それより前の通知は全部 channel に入っている

応答の**後**の通知は、読み切りの時に届いていれば控えに、届いていなければ会話中の event になる。agy が応答の後に送るのは usage / commands / session_info（状態だけ）なので、どちらでも結果は同じ。

実物で数を当てた結果は「実測の記録」の「大きい会話の load」。

#### 流れの訳し方

実測（agy 1.5.11、「実測の記録」の節）:
- 過去の通知は load の応答の**前**に、起きた順で流れる
- turn の区切りの通知は無い。`tool_call_update` も出ない（`tool_call` 1 件に status と結果まで入っている）
- 応答の**後**に usage_update / available_commands_update / session_info_update が 1 件ずつ来る

訳し方:
- **turn の区切り**: `user_message_chunk` が来たら新しい turn が始まったとみなす。履歴には `TurnCompleted` を入れない（VP の replay と同じ。終わりは `ReplayEnd` が下ろす）
- **user の発話**: `user_message_chunk` を `UserMessage` にする
  - `messageId` が付いていて、直前の chunk と同じなら、同じ発話の続きとして控えで 1 つにまとめる（opencode 2.3.0 は付ける。VP の acp-stage2 lane の実測、2026-11-20）
  - `messageId` が無ければ、続けて来ても別の発話にする（agy 1.5.11 は付けない。同じ発話の続きか別の発話かを見分けられない。答えの無いまま次の発話が来る並びも実測にある）
  - 間に会話の中身（本文・thinking・tool・plan）が挟まったら、同じ id でも続きにしない。usage / session_info などの状態の通知は挟まっても続きのまま（opencode は会話中に `session_info_update` を 1 turn に 20 件ほど流す）
  - opencode が 1 つの発話を複数の chunk で流す実例は、まだ取れていない（実測は 1 発話 1 chunk）。まとめる処理は偽の agent のテストでだけ確かめている
- **本文と thinking**: 続く chunk は控えの中で 1 件にまとめる
- **tool**: `tool_call` の status が completed / failed なら、`ToolCall` と `ToolCallUpdate` の 2 つにする。会話中も同じ規則
  - agy と grok の load は、結果まで入った `tool_call` 1 件で流す。opencode 2.3.0 の load は、`tool_call`（status=pending）と `tool_call_update` の 2 段で流す（会話中と同じ形）。どちらも控えでは 1 組の `ToolCall` + `ToolCallUpdate` になる
- **履歴に入れないもの**: usage / commands / 設定 / session_info は状態として持つだけ（今と同じ）

#### 会話中の `user_message_chunk`

agy 1.5.9 は会話中も、送った発話を `user_message_chunk` で返してきた（VP design 88 §1）。1.5.11 の実測では返さなかった。どちらでも同じ結果になるように決める。

- **host が出した prompt の turn の中**: broadcast に出さない。UI は送信の時に自分で発話を表示しているので、二重になる
- **控えには、送信の時に text を `UserMessage` として入れる**。返ってくる chunk には頼らない（返さない engine があっても控えが欠けない）
- **host が prompt を出していないのに来た時**: `UserMessage` として broadcast にも出す（他の書き手の発話。stdio の 1 接続では起きないはずだが、捨てない）

#### まだ決められないこと（実測してから書く）

- **console の TUI を開いたままの時**: 実測は「別 process が 1 turn 進めて終了する」形（`agy --conversation <id> -p`）で行った。TUI が開いたまま、acp 側が進めた分を TUI がどう扱うかは測っていない。向きが逆なだけで同じことが起きる見込みだが、こちらからは直せない
- **grok / opencode**: load の流れ、2 人の書き手の時の動き
- **画像つきの発話**が流れでどう見えるか（`UserMessage` は text だけ）
- **中断した turn** の見え方

### 公開 API

VP の `AcpAgentHost` に近い形にして、Stage 2 の差し替えを小さくする。実装は `crates/acp/src/host.rs`。下は要点で、正は code。

- `submit` / `interrupt` / `stop` は待たない（命令を queue に入れるだけ）。turn の最中の `submit` は、終わってから順に送る
- **受け取ってまだ送っていない発話も、`history()` / `attach()` の末尾に入る**（待ちの順の `UserMessage`）。待ちがあれば `in_flight` は true（これから turn が走る）。送った時に待ちから控えへ移るので、同じ発話が 2 回出ることは無い。`interrupt()` で捨てた分は消える
  - 理由: 呼び手は控えで画面を作り直す（`ReplayStart` で表示を消す）。会話の確定（1.5〜2 秒）や設定の当て直しを待っている間に利用者が送った発話は、まだ engine に送っていないが、利用者から見れば送った発話。控えに無いと画面から消え、次の答えが前の答えの吹き出しにつながって見える（VP の acp-stage2 lane の自己レビューが見つけた、2026-11-20）
  - 待ちの発話は控えの上限（`HistoryLimits`）の勘定の外。画像は入らない（`UserMessage` は text だけ）
  - `restart()` の命令より後に `submit` された発話は、命令の列に残り、立て直した接続で送られる。その間も待ちに入っているので、`restart()` が返す控えの末尾に出る（意図した動き。テストは無い）
- **会話の確定の前の `interrupt()` は、「送ってから止める」になる**（VP の手書きの host との違い）。`interrupt()` は命令の列に並ぶので、確定の前に「`submit(A)` → `interrupt()`」と呼ぶと、確定の後に「A を受け取る → turn の最中でないのですぐ A を送る → 中断（A の turn を `session/cancel`）」の順で処理される。A は engine に届き、控えにも入る。VP の手書きの host は、確定の前でも待ちをその場で捨てていたので、A は送られなかった。起きるのは「chat を開いて 2 秒ほどの間に送って、すぐ止めた」時だけなので、今は直していない（code を読んで確かめた。この順を見るテストは無い）。`stop()` は別で、確定の前でも先に効く
- `interrupt()` は、今の turn を中断し（`session/cancel`）、待たせていた発話も捨てる。turn の最中でなくても待ちは捨てる。捨てた発話は送られず、控えにも入らない（控えには送信の時に入れるため）。捨てた件数は返さない
- **中断に答えない engine は止める**。engine が `HostOptions::cancel_grace`（既定 3 秒）の間に中断した turn を閉じなければ、engine が固まったとみなし、host は process を止めて終了し（呼び手が作り直す）、`TurnCompleted`（turn を閉じる）→ `EngineExited` を出す。`TurnCompleted` が出るのは中断から 3〜4 秒後（期限 3 秒の後、stderr の読み切りを最大 1 秒待ってから出す）。期限の数え始めは最初の中断で、中断を重ねても延ばさない。期限は turn の終わりで下ろし、次の turn へ持ち越さない。期限の待ちの間に受け取った発話は送らず、`TurnCompleted` の後、`EngineExited` の前に `Error` で件数を知らせる
  - 理由: agy が thinking のまま 3000 秒以上固まり、停止を押しても `session/cancel` に答えず、turn が閉じなかった。呼び手（VP の GUI）は turn の終わりを見るまで「応答中」のままで、次の発話も送れなかった（2026-11-24、mako の報告）。実測の応答は 2〜14 ms（実測 5）なので、答える engine を 3 秒で止めることは無い見込み。ただし grok / opencode の中断の速さと、長い tool の実行中の中断の速さは測っていない
  - `TurnCompleted` は host が終了した後（`submit` が Err を返す状態）に出す。それを見て呼び手が次の発話を送っても、固まった engine に積まれて黙って失われることが無く、Err を見て host を作り直せる。この順は同じ task の中の並びで守っている（テストは current_thread の runtime なので、並びの崩れを赤にできない）
  - 期限の前に engine が終了した・接続が切れた時は、今までどおり `EngineExited` だけで、`TurnCompleted` は出ない。turn の終わりとして扱うのは呼び手（VP は `EngineExited` も turn の終わりとして扱う）
  - 止めるのは engine の process だけ。engine が起こした tool の子 process（process group）は止めない（`stop()` と同じ、今までからの課題）
- 失敗は `HostError`（`Spawn` / `Establish` / `Dead` / `Busy` / `Engine`）
- `spawn_with(connector, cwd, options)`: 接続の作り方（`Connector`）を差し替える口。既定は process の起動。テストは偽の agent をつなぐ。`restart()` はこの口をもう一度呼ぶ
- permission の既定の方針は `permission::allow`（kind で allow_once → allow_always、無ければ取り消し）
- `attach()`: 控えへの追加と event の送信を同じ lock の中で行うので、控えに入った event は受け口に出ず、控えに無い event は必ず受け口に出る。`history()` と `subscribe()` を別々に呼ぶと、その間の event が重なるか抜ける
  - 受け口を差し替える呼び手への注意: 控えに入らない event（`TurnCompleted` / `SessionInit` / `Error` など）は、古い受け口を最後に読んでから `attach()` までの間に出ると、古い受け口にだけ残って落ちる。turn の状態は `attach()` が返す `in_flight` に合わせ、`SessionInit` は `session_init()` で取り直す（VP の acp-stage2 lane が multi-thread の runtime で踏んだ所）
- 設定の意図: 会話中の `set_config` が成功したら、その値を今の意図として覚える。`restart()` の後の当て直しは今の意図から行う（起動の時の `config_intent` に戻さない）
- `restart()` が `Busy` を返すのは、turn の最中・送っていない発話がある時・設定の切替の応答を待っている時
- `stop()` と drop は、会話の確定や設定の当て直しを待っている最中でも、すぐに process を止める。止める合図は命令の queue とは別に持つので、先に `submit` してまだ送っていない発話より先に効く（止めたはずの engine に発話を送ってから止める、が起きない）。止めた時は `EngineExited` を出さない
- engine の終了や確定の失敗を見た時点で、`submit()` は Err を返し、`is_finished()` は true になる（stderr の読み切りを待つ前に）。呼び手は Err を見て host を作り直せる。`EngineExited` と終了の文（stderr の末尾つき）は、読み切りの後に出る
- 会話を確定できなかった時は、`EngineExited` の前に、理由を `Error` で出す。待っていた発話が無くても出す（`EngineExited` の文を画面に出さない UI でも、chat を開いただけの人に理由が見える。VP の手書きの host も initialize / session/new の失敗を `Error` で出していた）
- 送られなかった発話は黙って捨てない。確定の失敗や engine の終了で、受け取ってまだ送っていない発話があれば、`Error` に件数を添える。engine の終了は、待ちがある時だけ `Error` を出す。確定の後に接続そのものが失敗で終わった時（書き込みの失敗など）も数える。`stop()` と `interrupt()` で捨てた分は出さない（呼び手が自分で止めたため）
- `is_finished()`: host が終了したかを、待たずに読む。true ならその host はもう使えない。立て直しの途中は false
- `pid()`: engine の process の pid。`restart()` で process が替わったら新しい pid、engine が終了したら None。process を持たない `Connector` は None（VP は chat engine の pid を lane の実体の印に使う）
- `HostOptions::on_stderr`: engine の stderr を 1 行ごとに受け取る関数（既定は何もしない）。空行は渡さず、UTF-8 でない byte は置き換える。改行の無い出力は 8 KiB で区切って渡す。終了の文に付ける末尾 4 KB の保持は別に続けている。8 KiB で区切る時は文字の途中で切れて、置き換え文字が 1 つ入ることがある（log 用なので許容）。接続のたびに末尾の buffer を新しくし、前の process の読み手を止めるので、立て直しの後に前の process（やその孫）が遅れて書いた分は、新しい process の末尾にも `on_stderr` にも混ざらない。`spawn_with` で自前の connector を渡す時は、`ProcessConnector::on_stderr` で渡す
- 確定の前に接続が切れたら `Establish`、確定の後なら `Dead`。engine が initialize を読む前に終了した時、切れ方は OS と時機で変わる（EOF が先か、書き込みの `Broken pipe` が先か）が、どちらでも `Establish` で返す
- 終了の文に付ける stderr の末尾は、stderr の読み切りを 1 秒まで待ってから取る（engine が終了した直後は、最後に書いた分をまだ読んでいないことがある）。`restart()` と `stop()` では待たない
- アプリが渡した関数（`on_session` / `permission`）が panic して host の task が落ちても、待っている呼び手は `Dead` を受け取り、`EngineExited` が出る

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
    /// 起動から会話の確定（initialize と load / new）までの上限（既定 30 秒）
    pub establish_timeout: Duration,
    /// 設定の応答を待つ上限（既定 15 秒）。当て直しは全体で、会話中の切替は 1 回ごと
    pub config_timeout: Duration,
    /// 中断の後、engine が turn を閉じるのを待つ上限（既定 3 秒）。超えたら process を止め、TurnCompleted → EngineExited を出す
    pub cancel_grace: Duration,
    /// spawn 後に当て直す (config id, 値)。agy は load の後に既定へ戻る（VP design 88 §6.1）
    pub config_intent: Vec<(String, String)>,
    pub permission: Arc<dyn Fn(&RequestPermissionRequest) -> PermissionDecision + Send + Sync>,
    pub on_session: Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>,
    /// engine の stderr を 1 行ごとに受け取る（既定は何もしない）
    pub on_stderr: Arc<dyn Fn(&str) + Send + Sync>,
    /// 控えの上限（既定は CodexHistory と同じ: 1 MiB / 800 event / 1 件 32 KiB）
    pub history_limits: HistoryLimits,
}

impl AcpClientHost {
    pub fn spawn(launch: LaunchContext, options: HostOptions) -> Result<Self>;
    pub fn spawn_with<C: Connector>(connector: C, cwd: PathBuf, options: HostOptions) -> Result<Self>;
    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent>;
    /// 画像は {media_type, data(base64)}（VP の ImageInput / codex_input と同じ形）
    pub fn submit(&self, text: &str, images: &[PromptImage]) -> Result<()>;
    pub fn interrupt(&self) -> Result<()>;
    pub async fn set_config(&self, id: &str, value: &str) -> Result<Vec<AcpConfigOption>>;
    pub fn config_view(&self) -> Option<AcpConfigView>;
    pub fn session_init(&self) -> Option<ConversationEvent>;
    /// engine の process の pid（process を持たない connector と、終了の後は None）
    pub fn pid(&self) -> Option<u32>;
    /// 履歴の控え。会話の確定まで待つ。呼び手が ReplayStart 〜 ReplayEnd で包む
    pub async fn history(&self) -> Result<HistorySnapshot>;
    /// 控えと、その続きの event の受け口を一緒に取る（turn の最中に開き直す時）
    pub async fn attach(&self) -> Result<(HistorySnapshot, broadcast::Receiver<ConversationEvent>)>;
    /// process を立て直して load し直す。他の書き手が会話を進めた後、次の prompt の前に呼ぶ。turn の最中は Err
    pub async fn restart(&self) -> Result<HistorySnapshot>;
    pub fn stop(&self);
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
| `user_message_chunk` | load の流れ: `UserMessage`。会話中: host が出した prompt の turn の中なら出さない、そうでなければ `UserMessage` |
| `session_info_update` | 無視（title は未対応） |
| `notice` / `compaction_*` | 無視。client capability を宣言しないので来ない（agent は宣言した client にだけ送る） |
| 知らない種類 | log を出して無視 |

prompt の応答は `TurnCompleted` にする（context の使用量を載せる）。エラーなら `Error` を出してから `TurnCompleted`。

## Implementation

### Stage 1 でやったこと

1. crate `crates/acp`（`agent-chat-acp`）。agent-chat の gate（fmt / clippy `-D warnings` / `test --all-features` / 生成物の drift）に乗っている
2. calculations: 翻訳・設定・付随情報・permission・履歴の控え。fixture は VP design 88 §1・§6 の実測 wire（agy 1.5.9）と、今回の実測の形（agy 1.5.11）
3. 偽の agent のテスト（`tests/fake_agent.rs`、55 本）
   - 相手は SDK の `Channel::duplex()` の片側で、生の JSON-RPC を台本どおりに返す。SDK の `Agent.v1()` は使わなかった（load の応答の前後に通知を置く順序を、生の方が直接書ける）
   - 場面: 新規の turn / load の流れが履歴になる / turn の最中の `history()` と中断 / 発話の写し / `restart()` / load の失敗（既定と新規）/ 待ちの上限 / 設定の当て直しと切替 / 知らない update / EOF / prompt の error / permission の方針 / 発話の順送りと画像 / `SessionInit` の保留 / 会話 ID の callback / 控えの上限 / 会話中に切り替えた設定が立て直しの後も残る / 立て直しの失敗 / 渡した関数の panic / `stop()` / 他の書き手の発話 / `attach()` / process の起動の失敗と stderr の末尾 / 設定の当て直しに答えない engine / pid / stderr の行渡し / 同じ messageId の発話 / 確定の前に接続が切れる場面 / 中断で待ちの発話を捨てる / pending の tool_call と update の 2 段の load / 確定の前と当て直しの最中の stop と drop / 終了の後の submit / 送られなかった発話の通知 / 未知の kind の permission / まだ送っていない発話が控えに入る / 中断に答えない engine（待ち切りで閉じる・中断を重ねても延ばさない・次の turn へ持ち越さない・待ちの間の発話を知らせる）
4. 実機 E2E（`tests/agy_e2e.rs`、`#[ignore]`、4 本 + 実物の会話を load して数える 1 本）: agy 1.5.11、model は `gemini-3.8-flash`
5. VP の `AcpAgentHost` との比較（「VP の手書きの host との比較」の節）

実機 E2E の回し方:

```sh
cargo test -p agent-chat-acp --test agy_e2e -- --ignored --nocapture --test-threads=1
```

### 実機で測った項目

agy 1.5.11、2026-11-18、この host 経由。

| # | 項目 | 結果 |
|---|---|---|
| 1 | initialize の protocolVersion | SDK 経由で通った（agy は 1 を返す） |
| 2 | 型で読めるか | 応答（initialize / new / load / set_config_option / prompt）は SDK の型で読めた。通知は host が生の JSON で読むので、型では読んでいない。SDK の warn の log は確認していない（log を出して回していない） |
| 3 | prompt の応答の後の update | turn の終わりの後 2 秒待っても、何も来なかった。最初の turn の途中に `available_commands_update` が来る（`SessionInit` の出し直しになる）。本文が空のまま turn が終わる件（VP design 88 §6.4）は起きなかった |
| 4 | load の replay | 応答の前に届いた分は、`start_session()` が返った時点で stream に溜まっていた。その場で読み切って控えにできた。起動から確定まで 1.5〜2.1 秒（新規・load・立て直しとも） |
| 5 | cancel | `session/cancel` から 2〜14 ms で turn が終わった。終わりの後に本文は来なかった。permission の要求との順序は測れていない（下の「permission」） |
| 6 | stdout の中身 | JSON でない行で接続が切れることは無かった（4 本の E2E、約 10 turn） |
| 7 | 遅い handler | 測っていない。permission の判断の関数で重い処理をしない、という使い方の決まりのまま |
| 8a | 画像つきの発話 | 送れて、答えも返った。load の流れでは text だけが戻る（画像は `UserMessage` に出ない） |
| 8b | 中断した turn | load の流れに、途中までの thinking と本文が残る。会話中の控えと同じ並び（U t A） |
| 8c | TUI を開いたままの時、grok / opencode | 測っていない |

permission:
- 標準に無い kind の option: SDK の `PermissionOptionKind`（schema 1.18.0）は allow_once / allow_always / reject_once / reject_always の 4 つだけで、知らない値を受ける枝が無い。engine が他の kind を出すと、要求は型で読めず、注入した方針には届かない。その要求は session の流れに回り、host が「未対応の要求」として error（method not found）で断る。黙って待たせることは無いが、許可もされない（偽の agent のテストで確認。実機でこの形は見ていない）
- `--dangerously-skip-permissions` を付けずに file を作らせたが、agy は permission を求めずに実行した（mode は default。この環境の agy の設定によるものかは確かめていない）
- なので、実機では permission の要求を 1 度も受けていない。要求への答え方は偽の agent のテストだけで確かめている

設定:
- `set_config()` が `config_timeout` で時間切れになった時は、呼び手に Err を返すが、`config_view().error` には書かない（engine が error を返した時は書く）。VP の手書きの host は時間切れも設定面に書いていた
- 切替（thought_level）は成功し、応答の設定面で値が変わった。不正な値は engine の文（`Unknown model ID`）で失敗し、`config_view().error` に残った
- 「load の後に既定へ戻る」（VP design 88 §6.1）は、今回は確かめられなかった（既定の model が指定と同じ `gemini-3.8-flash` だった）

### VP の手書きの host との比較

同じシナリオを、VP の `AcpAgentHost`（nightly `504859ba`）とこの host で回し、broadcast に出た event の並びを比べた。agy 1.5.11、model は `gemini-3.8-flash`、2026-11-18。VP の側は、nightly の写しを repo の外に取り出して比較用のテストを 1 本足して回した（VP の repo は変えていない）。

記号: Init = `SessionInit`、A = 本文、t = thinking、c = `ToolCall`、u = `ToolCallUpdate`、End = `TurnCompleted`。本文の chunk の数は回ごとに変わる。

| 場面 | VP の `AcpAgentHost` | この host |
|---|---|---|
| 新規の 1 turn 目 | Init A Init End | Init A Init End |
| 2 turn 目 | A End | A End |
| 止めて再開（load）した直後 | Init Init | Init Init |
| load の後の turn（前の 2 turn を覚えているか） | A End（覚えている） | t A End（覚えている） |
| 長い turn を中断 | Init t t A → 中断 → Init End（3〜10 ms） | Init t A → 中断 → Init End（2〜14 ms） |
| tool（file を作る） | Init t c u t A A A Init End | Init t c u A A A A Init End |
| 過去の会話の履歴 | 出ない（replay_log だけ） | `history()` で U A U A が戻る |

同じだった所:
- `stop()` で process をその場で止める。この host は最初、止める命令を命令の loop でしか読まず、確定の前（上限 30 秒）と当て直しの最中（上限 15 秒）は process が残った。同じ会話 id で別の process が立つと「2 人の書き手」になる。VP の acp-stage2 lane の自己レビューが見つけた（2026-11-20）
- 中断で、待たせていた発話も捨てる（VP の codex / vpcode / 手書きの ACP の host は、どれも中断で待ち行列を空にする）。この host は最初、`session/cancel` を送るだけで待ちを残していた。比較のシナリオに「待ちがある状態での中断」が無く、VP の acp-stage2 lane が見つけた（2026-11-20）。lead の判断で VP に合わせた
- turn の中の並び、tool の名前（`File hello.txt`）と結果の形、中断の速さ
- 最初の turn の途中で `available_commands_update` が来て、`SessionInit` が turn の終わり（`TurnCompleted` の直前）に出し直される所

違う所（意図したもの）:
- **履歴**: この host は load の流れを `history()` で渡す。VP は捨てる
- **load の失敗**: この host の既定は失敗として返す。VP は黙って新規にする（`LoadFallback::NewSession` を渡せば同じになる）
- **permission の既定**: 許可の option が無い時、この host は取り消す。VP は先頭の option を選ぶ
- **会話中の `user_message_chunk`**: どちらも出さない（結果は同じ）。この host は、自分が prompt を出していない時に来た分だけ `UserMessage` で出す
- **結果まで入った `tool_call`**: この host は `ToolCallUpdate` も続ける。VP は `ToolCall` だけ（会話中は agy が update を別に送るので、差は load の流れでしか出ない）

測り方の注意:
- 1 回ずつの比較で、同じ入力でも model の答え方（thinking の有無、chunk の数）は回ごとに変わる。上の表の t と A の数の差は、host の差ではない
- 画像と設定の切替は、VP の側では回していない（VP の既存の実機テスト `e2e_real_agy_image_prompt` / `e2e_real_agy_config_set_and_reapply` がある）

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

- **`serde_json` の `preserve_order`**: SDK（`agent-client-protocol` 5.3.0 と `agent-client-protocol-schema` 1.18.0）が有効にする（`cargo tree -e features -i serde_json` で確認）。feature は build 全体で 1 つにまとまるので、この crate に依存する binary では `serde_json::Value` の key の順が「辞書順」から「入れた順」に変わる。agent-chat の中では、protocol の fixture の key を辞書順に書いて生成物を変えないようにした。consumer への影響（JSON の文字列を比べている所）は Stage 2 の前に consumer 側で調べる
- **SDK の版は consumer の lock で決まる**: path 依存は agent-chat の `Cargo.lock` を連れていかない。consumer が何もしないと新しい版（5.4.0 / schema 1.19.0 など）を引く。VP は lock で 5.3.0 / 1.18.0 に揃えている。agent-chat が版を上げる時は consumer に知らせる
- **SDK の版の追従**: 5.x で、schema は `=` で固定され、minor の版でも動く。unstable の feature は使わない。版は workspace の 1 か所で持つ
- **境界**: process を共有側が持つ最初の host になる。load の失敗の扱いが、VP の今（黙って新規）と design 86（変換しない）で食い違う。方針を注入にして既定は `Fail`、VP は `NewSession` を渡す
- **GFP**: crate を分けるので、GFP が依存するまで build に影響しない
- **Windows**: tokio::process と compat は Windows でも動く想定。未確認

### Stage 2 で決めること

- VP の切替スイッチの形（env か settings か）。最初は agy だけ
- submodule の revision を動かすので、GFP と同じ revision に揃える（design 86 の gate）
- VP の `AcpAgentHost` を残す期間（比較のために並べて置く → 置き換える）

2 人の書き手（試験用に作った会話。model は gemini-3.8-flash）:

手順: 生きた `agy acp`（A）で会話を作り 1 turn 目（合言葉 1 を覚えさせる）→ 別 process（`agy --conversation <id> -p`）で 2 turn 目（合言葉 2）→ A に戻る。

| 確かめたこと | 結果 |
|---|---|
| 別 process の turn の直後に、新しい process で load | 合言葉 1 と 2 の両方がある（別 process の分は保存されている） |
| A に `session/load` をもう一度送る | 成功するが、合言葉 2 の turn は流れてこない |
| A で「覚えた合言葉を全部挙げて」と聞く | 合言葉 1 だけを答える（load し直した後でも同じ） |
| その後に、新しい process で load | 合言葉 1 と A の 3 turn 目はある。**合言葉 2 の turn は消えている** |
| A の `session/list` の updatedAt | 別 process の turn の前後で変わらない |

- 2 回測って同じ結果だった（load し直しを挟む場合と挟まない場合）
- 会話中の turn に `user_message_chunk` は出なかった（1.5.9 の実測とは違う）
- load の応答の configOptions の model は、切り替えた値のままだった（この測り方では既定に戻らなかった。既定が同じ model だったので、戻るかどうかは判定できていない）

大きい会話の load（4924fbc2…、この host 経由、読むだけ。設定の当て直しはしていない）:

| 控えの上限 | user | thinking | 本文 | ToolCall | ToolCallUpdate | 計 | 大きさ | truncated |
|---|---|---|---|---|---|---|---|---|
| なし | 12 | 63 | 15 | 147 | 147 | 384 | 3,464,343 bytes | false |
| 既定（1 MiB / 800 件 / 1 件 32 KiB） | 12 | 63 | 15 | 147 | 147 | 384 | 1,037,291 bytes | true |

- 流れの件数（user 12 / tool_call 147 / thought 63 / message 16）と合う。本文が 15 なのは、続いた 2 chunk を 1 件にまとめたため。`tool_call` 147 件は、結果まで入っているので `ToolCall` と `ToolCallUpdate` の 147 組になる
- 確定の後 3 秒、受け口に出たのは `SessionInit` だけ。load の流れが会話中の event として漏れることは無かった
- 既定の上限でも **12 turn が全部残った**。落ちたのは、1 件 32 KiB を超えた分の先頭だけ（tool の結果など）。それで 3.46 MB が 1.04 MB になった
- ただし全体の上限（1,048,576 bytes）まで残り約 11 KB しか無い。この会話があと少し伸びると、古い turn から落ち始める
- 起動から確定まで 1.7〜2.0 秒

engine ごとの「2 人の書き手」（VP の acp-stage2 lane の実測、2026-11-20。opencode 2.3.0 と grok 1.0.89 は試験用の会話で 1 回ずつ。agy は上の記録）:

| 確かめたこと | opencode 2.3.0 | grok 1.0.89 | agy 1.5.11 |
|---|---|---|---|
| 生きた process に `session/load` をもう一度 | 2 人目の turn も流れる | 流れる | 流れない |
| 生きた process に「合言葉を全部挙げて」 | 全部答える | 2 人目の分を答えない | 自分の分だけ |
| その後、新しい process で load | 全部残る | 残る | 2 人目の分が消える |

- opencode は、生きた process が 2 人目の turn を知っていた。process が共有の background service につなぐ作りだから、というのは推測（`opencode --help` の文言から）
- `restart()` は 3 つのどれでも安全側に働く（opencode では要らないだけ）。engine ごとに呼ぶかどうかは呼び手が決める
- opencode は `user_message_chunk` / `agent_message_chunk` / `agent_thought_chunk` の全部に `messageId` を付ける（会話中も load でも）。1 つの発話が複数の chunk に分かれる例は取れていない（短い発話 4 つで 1 発話 1 chunk）。会話中は `user_message_chunk` が来ない
