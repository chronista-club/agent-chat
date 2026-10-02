//! Codex app-server の逆方向要求（質問・承認・MCP 手続き）の台帳。
//!
//! native ID と表示 ID は別の名前空間で、表示 ID は host 世代ごとに一意。判断（受理・許可可否・
//! 応答の wire 形）だけを持ち、タイトルや詳細のラベルなど表示の言い回しは各アプリの射影が持つ。
use std::collections::{BTreeMap, HashMap, HashSet};

use agent_chat_protocol::{CodexElicitation, CodexQuestion, QuestionOption};
use serde_json::{Map, Value, json};

use crate::codex_elicitation::{self, Elicitation};
use crate::codex_grants::Permissions;

const MAX_DETAIL_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 8;
const MAX_QUESTIONS: usize = 16;

/// 要求の種類。wire の `kind` はアプリの射影が決める（旧 UI との互換のため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionKind {
    Question,
    Command,
    /// `networkApprovalContext` を持つコマンド承認。
    Network,
    FileChange,
    Permissions,
    /// Codex の MCP ツール実行承認（空フォーム + `codex_approval_kind=mcp_tool_call`）。
    McpApproval,
    /// 一般の MCP フォーム / URL 手続き。turn を越えて残る。
    McpElicitation,
}

impl InteractionKind {
    pub fn is_mcp(self) -> bool {
        matches!(self, Self::McpApproval | Self::McpElicitation)
    }
}

/// 許可できない・回答できない理由。文言はアプリが持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// MCP 手続きの形式が未対応。中身は検証の説明。
    UnsupportedMcp(String),
    /// 追加権限の形式が未対応。中身は検証の説明。
    InvalidPermissions(String),
    /// コマンド承認の対象（コマンド・接続先・追加権限）が無い。
    MissingTarget,
    /// ファイル変更の差分が先行 item から取れない。
    MissingChanges,
}

/// 表示用の要求。回答本文は保存しない。
#[derive(Debug, Clone, PartialEq)]
pub struct Interaction {
    pub request_id: String,
    pub kind: InteractionKind,
    /// native の項目名のまま（`command` / `cwd` / `serverName` / `tool_params` / `changes` …）。
    pub detail: Map<String, Value>,
    pub notice: Option<Notice>,
    pub questions: Vec<CodexQuestion>,
    pub elicitation: Option<CodexElicitation>,
    pub server: Option<String>,
    /// native が decline を提示せず、拒否を cancel で返す要求。
    pub cancel_on_deny: bool,
    pub blocking: bool,
    pub can_accept: bool,
    /// 「この会話では許可」を返せる（MCP ツール承認で `_meta.persist` に `session` があるときだけ）。
    pub can_remember: bool,
}

/// 人の判断。`answers` は質問の回答（ID → 文字列）、MCP フォームの値、権限の部分選択。
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow {
        answers: Option<Value>,
    },
    /// この会話（Codex セッション）の間は同じツールを聞かない。永続設定（always）へは広げない。
    AllowSession,
    Deny,
    Cancel,
}

struct Pending {
    elicitation: Option<Elicitation>,
    permissions: Option<Permissions>,
    /// 要求された権限そのもの（「全部許可」で turn 限定に返す）。
    requested: Value,
    native_id: Value,
    turn: String,
    view: Interaction,
    responding: bool,
}

pub struct Interactions {
    generation: uuid::Uuid,
    next_id: u64,
    pending: BTreeMap<String, Pending>,
    file_changes: HashMap<String, Value>,
}

impl Default for Interactions {
    fn default() -> Self {
        Self {
            generation: uuid::Uuid::now_v7(),
            next_id: 0,
            pending: BTreeMap::new(),
            file_changes: HashMap::new(),
        }
    }
}

impl Interactions {
    pub fn snapshot(&self) -> Vec<Interaction> {
        self.pending.values().map(|p| p.view.clone()).collect()
    }

    pub fn get(&self, request_id: &str) -> Option<&Interaction> {
        self.pending.get(request_id).map(|p| &p.view)
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.file_changes.clear();
    }

    /// fileChange の差分は承認要求より前の `item/started` が供給する。
    /// 未回答が減ったら true（snapshot の再送が要る）。
    pub fn observe(&mut self, method: &str, params: &Value) -> bool {
        if method == "item/started"
            && params["item"]["type"] == "fileChange"
            && let (Some(turn), Some(id)) =
                (params["turnId"].as_str(), params["item"]["id"].as_str())
        {
            let changes = &params["item"]["changes"];
            if changes.is_array()
                && changes.to_string().len() <= MAX_DETAIL_BYTES
                && self.file_changes.len() < MAX_PENDING
            {
                self.file_changes
                    .insert(format!("{turn}/{id}"), changes.clone());
            }
        }
        let before = self.pending.len();
        if method == "serverRequest/resolved" {
            self.pending
                .retain(|_, p| p.native_id != params["requestId"]);
        } else if method == "turn/completed"
            && let Some(turn) = params["turn"]["id"].as_str()
        {
            self.pending
                .retain(|_, p| p.view.kind == InteractionKind::McpElicitation || p.turn != turn);
            self.file_changes
                .retain(|key, _| !key.starts_with(&format!("{turn}/")));
        }
        before != self.pending.len()
    }

    /// 要求を受理して表示 ID を返す。受理できない要求は Err（呼び出し側が native へ error を返す）。
    pub fn receive(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
        thread: Option<&str>,
        turn: Option<&str>,
    ) -> Result<String, String> {
        if !(id.is_string() || id.as_i64().is_some()) {
            return Err("不正な request ID".into());
        }
        let is_mcp = method == "mcpServer/elicitation/request";
        // MCP の turnId は相関情報で権限ではない（turn 外の手続きもある）。
        let valid_turn = if is_mcp {
            params
                .get("turnId")
                .is_some_and(|id| id.is_null() || id.as_str().is_some_and(|s| !s.is_empty()))
        } else {
            turn.is_some() && params["turnId"].as_str() == turn
        };
        if thread.is_none_or(str::is_empty) || params["threadId"].as_str() != thread || !valid_turn
        {
            return Err("質問・承認の対象会話または turn が一致しません".into());
        }
        if self.pending.values().any(|p| p.native_id == *id) {
            return Err("重複した未回答 request ID".into());
        }
        if self.pending.len() >= MAX_PENDING || params.to_string().len() > MAX_DETAIL_BYTES {
            return Err("質問・承認が表示可能な上限を超えています".into());
        }
        let kind = match method {
            "item/tool/requestUserInput" => InteractionKind::Question,
            "mcpServer/elicitation/request" if is_tool_call_approval(params) => {
                InteractionKind::McpApproval
            }
            "mcpServer/elicitation/request" => InteractionKind::McpElicitation,
            "item/permissions/requestApproval" => InteractionKind::Permissions,
            "item/commandExecution/requestApproval"
                if params["networkApprovalContext"].is_object() =>
            {
                InteractionKind::Network
            }
            "item/commandExecution/requestApproval" => InteractionKind::Command,
            "item/fileChange/requestApproval" => InteractionKind::FileChange,
            _ => return Err(format!("この要求には未対応です: {method}")),
        };
        let mut view = Interaction {
            request_id: String::new(),
            kind,
            detail: Map::new(),
            notice: None,
            questions: Vec::new(),
            elicitation: None,
            server: params["serverName"].as_str().map(str::to_owned),
            cancel_on_deny: false,
            blocking: kind != InteractionKind::Question
                || params["isBlocking"].as_bool().unwrap_or(true),
            can_accept: true,
            can_remember: false,
        };
        let mut elicitation = None;
        let mut permissions = None;
        match kind {
            InteractionKind::Question => view.questions = parse_questions(&params["questions"])?,
            InteractionKind::McpApproval => {
                copy(&mut view.detail, params, &["serverName", "message"]);
                copy(
                    &mut view.detail,
                    &params["_meta"],
                    &["tool_title", "tool_description", "tool_params"],
                );
                view.elicitation = Some(CodexElicitation {
                    server_name: view.server.clone().unwrap_or_default(),
                    message: params["message"].as_str().unwrap_or_default().into(),
                    url: None,
                    fields: Vec::new(),
                });
                // always は個人の Codex 設定へ書かれうるので扱わない（会話が権限を所有する）。
                view.can_remember = params["_meta"]["persist"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v == "session"));
            }
            InteractionKind::McpElicitation => {
                copy(&mut view.detail, params, &["serverName", "message"]);
                match Elicitation::parse(params) {
                    Ok(parsed) => {
                        view.elicitation = Some(parsed.view.clone());
                        elicitation = Some(parsed);
                    }
                    Err(message) => {
                        view.can_accept = false;
                        view.notice = Some(Notice::UnsupportedMcp(message));
                    }
                }
            }
            InteractionKind::Permissions => {
                copy(
                    &mut view.detail,
                    params,
                    &["environmentId", "cwd", "reason", "permissions"],
                );
                match Permissions::parse(&params["permissions"]) {
                    Ok(parsed) => {
                        view.questions = parsed.questions();
                        permissions = Some(parsed);
                    }
                    Err(message) => {
                        view.can_accept = false;
                        view.notice = Some(Notice::InvalidPermissions(message));
                    }
                }
            }
            InteractionKind::Command | InteractionKind::Network | InteractionKind::FileChange => {
                copy(
                    &mut view.detail,
                    params,
                    &[
                        "command",
                        "cwd",
                        "reason",
                        "environmentId",
                        "networkApprovalContext",
                        "additionalPermissions",
                        "grantRoot",
                    ],
                );
                if kind == InteractionKind::FileChange {
                    let key = format!(
                        "{}/{}",
                        turn.unwrap_or_default(),
                        params["itemId"].as_str().unwrap_or_default()
                    );
                    if let Some(changes) = self.file_changes.remove(&key) {
                        view.detail.insert("changes".into(), changes);
                    } else {
                        view.can_accept = false;
                        view.notice = Some(Notice::MissingChanges);
                    }
                } else {
                    if let Some(choices) = params.get("availableDecisions").filter(|v| !v.is_null())
                    {
                        let choices = choices.as_array().ok_or("不正な承認選択肢")?;
                        view.can_accept = choices.contains(&json!("accept"));
                        if !choices.contains(&json!("decline")) {
                            if !choices.contains(&json!("cancel")) {
                                return Err("拒否または中断をサポートしない承認要求".into());
                            }
                            view.cancel_on_deny = true;
                        }
                    }
                    let has_target = params["command"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty())
                        || params["networkApprovalContext"]["host"]
                            .as_str()
                            .is_some_and(|s| !s.trim().is_empty())
                        || params["additionalPermissions"]
                            .as_object()
                            .is_some_and(|p| !p.is_empty());
                    if !has_target {
                        view.can_accept = false;
                        view.notice = Some(Notice::MissingTarget);
                    }
                }
            }
        }
        self.next_id += 1;
        let request_id = format!("codex:{}:{}", self.generation, self.next_id);
        view.request_id = request_id.clone();
        self.pending.insert(
            request_id.clone(),
            Pending {
                elicitation,
                permissions,
                requested: params["permissions"].clone(),
                native_id: id.clone(),
                turn: turn.unwrap_or_default().into(),
                view,
                responding: false,
            },
        );
        Ok(request_id)
    }

    /// native へ返す JSON-RPC 応答（`{"id","result"}`）。検証だけで要求は消費しない。
    pub fn response(&self, request_id: &str, decision: &Decision) -> Result<Value, String> {
        let pending = self
            .pending
            .get(request_id)
            .ok_or("この質問・承認はすでに終了しているか、別の会話のものです")?;
        let view = &pending.view;
        let result = match (view.kind, decision) {
            (_, Decision::AllowSession) => {
                if view.kind != InteractionKind::McpApproval || !view.can_remember {
                    return Err("この要求は会話中の許可にできません".into());
                }
                json!({"action":"accept","_meta":{"persist":"session"}})
            }
            (InteractionKind::McpApproval, Decision::Allow { answers }) => {
                // 空フォームなので値は受けない。UI が送る空の回答・accept 表明だけ許す。
                let empty = answers.as_ref().is_none_or(|a| {
                    a.as_object().is_some_and(|o| {
                        o.is_empty() || (o.len() == 1 && o.get("action") == Some(&json!("accept")))
                    })
                });
                if !view.can_accept || !empty {
                    return Err("この要求は許可できません".into());
                }
                json!({"action":"accept"})
            }
            (InteractionKind::McpApproval, Decision::Deny) => json!({"action":"decline"}),
            (InteractionKind::McpApproval, Decision::Cancel) => json!({"action":"cancel"}),
            (InteractionKind::McpElicitation, Decision::Allow { answers }) => {
                if !view.can_accept {
                    return Err("この MCP 手続きには回答できません".into());
                }
                pending
                    .elicitation
                    .as_ref()
                    .ok_or("この MCP 手続きには回答できません")?
                    .accept(answers.as_ref())?
            }
            (InteractionKind::McpElicitation, Decision::Deny) => {
                codex_elicitation::response("decline", Value::Null)
            }
            (InteractionKind::McpElicitation, Decision::Cancel) => {
                codex_elicitation::response("cancel", Value::Null)
            }
            (InteractionKind::Permissions, Decision::Allow { answers }) => {
                let grants = pending
                    .permissions
                    .as_ref()
                    .filter(|_| view.can_accept)
                    .ok_or("この要求は許可できません。内容を確認し拒否してください。")?;
                match answers {
                    // 部分選択なし = 要求された権限をそのまま、この turn だけ。
                    None => json!({"permissions":pending.requested,"scope":"turn"}),
                    Some(_) => grants.response(answers.as_ref())?,
                }
            }
            (InteractionKind::Permissions, Decision::Deny | Decision::Cancel) => {
                json!({"permissions":{},"scope":"turn"})
            }
            (InteractionKind::Question, Decision::Allow { answers }) => {
                let input = answers
                    .as_ref()
                    .and_then(Value::as_object)
                    .ok_or("質問への回答がありません")?;
                if input.len() != view.questions.len() {
                    return Err("質問と回答の数が一致しません".into());
                }
                let mut mapped = Map::new();
                for q in &view.questions {
                    let answer = input
                        .get(&q.id)
                        .and_then(Value::as_str)
                        .filter(|a| !a.trim().is_empty() && a.len() <= MAX_DETAIL_BYTES)
                        .ok_or("未回答または長すぎる回答があります")?;
                    mapped.insert(q.id.clone(), json!({"answers":[answer]}));
                }
                json!({"answers":mapped})
            }
            (InteractionKind::Question, Decision::Deny | Decision::Cancel) => {
                json!({"answers":{}})
            }
            (_, Decision::Allow { .. }) => {
                if !view.can_accept {
                    return Err("この要求は許可できません。内容を確認し拒否してください。".into());
                }
                json!({"decision":"accept"})
            }
            (_, Decision::Deny) if view.cancel_on_deny => json!({"decision":"cancel"}),
            (_, Decision::Deny) => json!({"decision":"decline"}),
            (_, Decision::Cancel) => json!({"decision":"cancel"}),
        };
        Ok(json!({"id":pending.native_id,"result":result}))
    }

    /// 応答を組み立て、書き込みが終わるまで二重回答を止める。検証エラーでは要求を消さない。
    pub fn begin_response(
        &mut self,
        request_id: &str,
        decision: &Decision,
    ) -> Result<Value, String> {
        if self.pending.get(request_id).is_some_and(|p| p.responding) {
            return Err("回答を送信中です".into());
        }
        let reply = self.response(request_id, decision)?;
        if let Some(p) = self.pending.get_mut(request_id) {
            p.responding = true;
        }
        Ok(reply)
    }

    pub fn finish_response(&mut self, request_id: &str) {
        self.pending.remove(request_id);
    }

    /// 人の代わりに許可する応答。信頼する MCP サーバーのツール承認だけが対象。
    /// どの会話設定（フルアクセス等）で呼ぶかはアプリが決める。
    pub fn auto_accept(&self, request_id: &str, trusted_servers: &[&str]) -> Option<Value> {
        let view = self.get(request_id)?;
        (view.kind == InteractionKind::McpApproval
            && view.can_accept
            && view
                .server
                .as_deref()
                .is_some_and(|s| trusted_servers.contains(&s)))
        .then(|| {
            self.response(request_id, &Decision::Allow { answers: None })
                .ok()
        })
        .flatten()
    }

    /// 中断（Cancel）で turn の interrupt も要る要求。command / file の cancel は Codex 自身が中断する。
    pub fn needs_interrupt(&self, request_id: &str) -> bool {
        self.get(request_id).is_some_and(|v| {
            matches!(
                v.kind,
                InteractionKind::Question
                    | InteractionKind::Permissions
                    | InteractionKind::McpApproval
                    | InteractionKind::McpElicitation
            )
        })
    }
}

/// Codex のツール実行承認は項目の無い object フォーム。一般フォームを空回答で許可しないための判定。
fn is_tool_call_approval(params: &Value) -> bool {
    let schema = &params["requestedSchema"];
    params["mode"] == "form"
        && params["_meta"]["codex_approval_kind"] == "mcp_tool_call"
        && schema["type"] == "object"
        && schema["properties"]
            .as_object()
            .is_some_and(|p| p.is_empty())
        && schema
            .get("required")
            .is_none_or(|v| v.is_null() || v.as_array().is_some_and(|a| a.is_empty()))
}

fn copy(detail: &mut Map<String, Value>, from: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(value) = from.get(*key).filter(|v| !v.is_null()) {
            detail.insert((*key).into(), value.clone());
        }
    }
}

fn parse_questions(rows: &Value) -> Result<Vec<CodexQuestion>, String> {
    let rows = rows
        .as_array()
        .filter(|rows| !rows.is_empty() && rows.len() <= MAX_QUESTIONS)
        .ok_or("質問がありません")?;
    let mut ids = HashSet::new();
    let mut questions = Vec::new();
    for q in rows {
        let id = q["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("質問 ID がありません")?;
        if !ids.insert(id) {
            return Err("質問 ID が重複しています".into());
        }
        let options = match &q["options"] {
            Value::Null => Vec::new(),
            Value::Array(options) => options
                .iter()
                .map(|o| {
                    Ok(QuestionOption {
                        label: o["label"]
                            .as_str()
                            .ok_or("選択肢 label がありません")?
                            .into(),
                        description: o["description"].as_str().unwrap_or_default().into(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
            _ => return Err("不正な質問選択肢".into()),
        };
        questions.push(CodexQuestion {
            id: id.into(),
            header: q["header"].as_str().unwrap_or_default().into(),
            question: q["question"].as_str().ok_or("質問文がありません")?.into(),
            options,
            is_secret: q["isSecret"].as_bool().unwrap_or(false),
            is_other: q["isOther"].as_bool().unwrap_or(false),
        });
    }
    Ok(questions)
}

#[cfg(test)]
mod tests;
