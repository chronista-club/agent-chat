# 01. 共有チャット基盤の開発

> **Status**: Draft — ローカル移行中
> **Related**: VP design 86（旧 74）、`mem_1CfcoK3QjHurVQ173tZDuA`

## 正本

Rust event と付随型は `crates/protocol/src/`。公開型を ts-rs で
`packages/chat/src/generated/` へ生成し、両 consumer が package から読む。
VP と GFP の旧 module path は互換 re-export であり、型を再定義しない。

Claude stream-json の翻訳は `crates/host/src/claude_translate.rs`。
本文・tool・usage・subagent・中断・transcript commit 境界を同じ実装で扱う。
承認 control は `control_events` / `unanswerable_control_request` で別に読む。
VP は既存の質問 router を保持し、GFP の adapter は control event と本文 event を合成する。
GFP の adapter が commit 境界を使うのは、process host / replay の移行時。

Codex app-server の逆方向要求（質問・承認・MCP 手続き）の台帳は `crates/host/src/codex_interactions.rs`。
受理・許可可否・native へ返す応答の形を持ち、タイトルや詳細のラベルは持たない。
各アプリはこの台帳を包み、自分のカード語彙（VP `CodexInteraction` / GFP `codex_requests::Request`）へ射影する。
MCP ツール実行承認（空フォーム）は `McpApproval`、一般フォーム / URL は `McpElicitation`。
会話内許可（`persist=session`）・信頼サーバーの自動許可・中断時の interrupt 判定も台帳が持つ。
どの会話設定で自動許可するか（GFP のフルアクセス）はアプリが決める。

Codex app-server の item 系通知（本文 delta / completed、reasoning、tool、MCP）の翻訳は
`crates/host/src/codex_rpc_translate.rs`。本文は item 単位の `CodexMessage`、MCP の tool 名は
`mcp__<server>__<tool>`（アプリが変更系 tool を名前で判定する）。thread / turn の lifecycle は各アプリの host。

## 型変更と検証

この repo の root で実行する。

```sh
cargo test --all-features
cargo run -p agent-chat-protocol --features typescript --example export-types
cargo fmt --all --check
```

`tests/fixtures.rs` が全 event kind の送信形を生成する。serde の
`default` と `skip_serializing_if`、ts-rs の `optional` を別々に扱う。
旧 payload の読取は `tests/contract.rs`、送り出す形は generated fixtures で検証する。
生成型を手編集しない。新しい公開型を足したら `packages/chat/src/protocol.ts` の export も加える。

両 consumer の WebView では、共有 package の更新後に以下を実行する。
Bun `file:` 依存はキャッシュされるので再導入を省かない。

```sh
bun install --force
./node_modules/.bin/tsc --noEmit
bun run build
```

VP の WebView は `bun run test conversation-event-contract.test.ts chatview.test.ts`、
GFP は `bun test src/shared-agent-chat.test.ts src/codex-interactions.test.ts src/echoes*.test.ts`。
各 Rust workspace では VP `cargo test -p vantage-point --lib conversation`、
GFP `cargo test -p gfp-app --lib echoes`。GFP は先に bundle を生成する。

## 配布前の gate

Codex のプロセス起動は `codex_process::command` を VP/GFP が共用する。
stdio の選択・pipe・kill-on-drop が共有側、CLI探索・cwd・identity env・MCP/rules の
明示的な `-c` override がアプリ側。共有側は権限モードを補わない。
`tests/codex_process.rs` は認証不要の実プロセスでこの境界を検証する。
native 履歴は `codex_history::CodexHistory` が検証と表示用の変換を所有する（VP接続済み、
GFPのnative履歴/UI接続は未移行）。空/重複IDの履歴を正常な空会話として受け入れない。

現在の Cargo path / Bun file は専用 worktree から共有 checkout を参照する開発依存。
公開名・license・正式 version・配布方式の決定後、両 consumer が同じ release を参照する
再現可能な依存へ置き換える。この状態で app release を作らない。
process host / reducer / Solid UI はまだ各 consumer にある。
protocol と Claude translator の共有だけではタスク完了でない。

GFP の native 入力境界は `codex_input` を使う。画像（base64・対応 MIME・decoded 5 MiB）、
queue 操作の対象と一覧ページの形を共有側で検証する。transport、期限、通知との直列化、
project/conversation 世代照合は consumer 側。現時点で VP の queue actor は独立実装のまま。
