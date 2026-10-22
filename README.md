# agent-chat

VP と GFP が使用する Claude / Codex のチャット基盤。
Rust の protocol / host と TypeScript の model / SolidJS UI を同じ repository で管理する。

現在は移行中。公開 package 名・配布は未確定。既存 app の権限設定は adapter が明示的に渡す。
会話 registry、案件 store、MCP の資格情報、WebView / Unison bridge は consumer が所有する。

設計: VP `docs/design/86-shared-agent-chat.md`（旧 74、nightly の 74-brand-identity と衝突したため繰り下げ）。
起票: `mem_1CfcoK3QjHurVQ173tZDuA`。

## 検証方針

Rust serde → 生成 TypeScript の契約を固定し、両 consumer の型検査とテストを行う。
resume 失敗で新しい会話を作らず、古い host の応答を新しい会話へ渡さない。
MCP 接続状態、会話内承認、engine 固有の能力を保持する。

この repository はまだ配布されていない。ローカル path 依存は開発用であり、
両 consumer の再現可能な配布依存への移行も完了条件に含む。

## ライセンス

MIT OR Apache-2.0 dual license（VP と同じ）。
[LICENSE-MIT](LICENSE-MIT) / [LICENSE-APACHE](LICENSE-APACHE)
