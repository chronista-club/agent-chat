//! 会話が所有する Codex の権限。個人 config.toml へは書かない。
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum CodexPermission {
    Ask,
    #[default]
    Standard,
    AutoReview,
    FullAccess,
}
impl CodexPermission {
    pub fn approval(self) -> &'static str {
        if self == Self::FullAccess {
            "never"
        } else {
            "on-request"
        }
    }
    pub fn reviewer(self) -> &'static str {
        if self == Self::AutoReview {
            "auto_review"
        } else {
            "user"
        }
    }
    pub fn sandbox(self) -> &'static str {
        match self {
            Self::Ask => "read-only",
            Self::FullAccess => "danger-full-access",
            _ => "workspace-write",
        }
    }
    pub fn policy(self, cwd: &str) -> Value {
        match self {
            Self::Ask => json!({"type":"readOnly"}),
            Self::FullAccess => json!({"type":"dangerFullAccess"}),
            _ => json!({"type":"workspaceWrite","writableRoots":[cwd],"networkAccess":false}),
        }
    }
}
