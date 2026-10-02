use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default, rename = "isSecret")]
    pub is_secret: bool,
    #[serde(default, rename = "isOther")]
    pub is_other: bool,
    #[serde(default)]
    pub options: Option<Vec<QuestionOption>>,
}
pub use crate::event::QuestionOption;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct Request {
    pub request_id: String,
    pub kind: String,
    pub title: String,
    #[cfg_attr(feature = "typescript", ts(type = "unknown"))]
    pub detail: Value,
    pub questions: Vec<Question>,
    pub can_accept: bool,
    /// 「この会話では許可」を出せるか。Codex が MCP ツール承認で `persist` に `session` を示したときだけ。
    #[serde(default)]
    pub can_remember: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum Response {
    Allow,
    /// この会話（Codex のセッション）の間は同じツールを聞かない。MCP ツール承認だけ
    AllowSession,
    Deny,
    Cancel,
    Answer {
        answers: BTreeMap<String, Vec<String>>,
    },
}
