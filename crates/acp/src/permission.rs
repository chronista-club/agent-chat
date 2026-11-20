//! `session/request_permission` への既定の答え方。

use agent_client_protocol::v1::schema::{PermissionOptionKind, RequestPermissionRequest};

/// permission の要求への答え。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// この optionId を選ぶ。
    Select(String),
    /// 選ばずに取り消す。
    Cancelled,
}

/// 既定の方針: 許可する。選ぶのは option の `kind` で、並び順や名前では選ばない。
/// 狭い方（`allow_once`）を先に、無ければ `allow_always`。許可の option が無ければ取り消す。
///
/// VP の手書きの host は「allow 系が無ければ先頭の option」だった。先頭が拒否でも選んで
/// しまうので、ここでは取り消しにする。
pub fn allow(request: &RequestPermissionRequest) -> PermissionDecision {
    [
        PermissionOptionKind::AllowOnce,
        PermissionOptionKind::AllowAlways,
    ]
    .iter()
    .find_map(|kind| request.options.iter().find(|o| o.kind == *kind))
    .map_or(PermissionDecision::Cancelled, |o| {
        PermissionDecision::Select(o.option_id.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(options: serde_json::Value) -> RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "sessionId": "s",
            "toolCall": {"toolCallId": "t1"},
            "options": options,
        }))
        .expect("permission の要求")
    }

    #[test]
    fn prefers_allow_once_then_allow_always_by_kind() {
        let both = request(serde_json::json!([
            {"optionId": "rej", "name": "No", "kind": "reject_once"},
            {"optionId": "always", "name": "Always", "kind": "allow_always"},
            {"optionId": "once", "name": "Yes", "kind": "allow_once"},
        ]));
        assert_eq!(allow(&both), PermissionDecision::Select("once".into()));
        let always = request(serde_json::json!([
            {"optionId": "rej", "name": "No", "kind": "reject_once"},
            {"optionId": "always", "name": "Always", "kind": "allow_always"},
        ]));
        assert_eq!(allow(&always), PermissionDecision::Select("always".into()));
    }

    #[test]
    fn cancels_when_no_allow_option() {
        let none = request(serde_json::json!([
            {"optionId": "first", "name": "No", "kind": "reject_once"},
        ]));
        assert_eq!(allow(&none), PermissionDecision::Cancelled);
        assert_eq!(
            allow(&request(serde_json::json!([]))),
            PermissionDecision::Cancelled
        );
    }
}
