//! 履歴の控え — `session/load` が流した過去の会話と、その後の会話中の event。
//!
//! 設計は `docs/design/01-acp-client-host.md` の「履歴」。呼び手は [`HistorySnapshot`] を
//! `ReplayStart` 〜 `ReplayEnd` で包んで配る。
//!
//! - turn の区切りは `UserMessage`（load の流れに区切りの通知は無い）
//! - 本文と thinking の chunk は、続く分を 1 つにまとめて持つ
//! - 上限（全体の byte 数・event の数・1 件の大きさ）を超えたら、古い turn から落とす

use std::collections::VecDeque;

use agent_chat_protocol::ConversationEvent;

/// 控えの上限。既定は Codex の履歴（`agent-chat-host` の `codex_history`）と同じ値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLimits {
    pub max_bytes: usize,
    pub max_events: usize,
    /// 本文 1 件 / tool の入力 1 件 / tool の結果 1 件の上限。超えたら末尾を残して切る。
    pub max_item_bytes: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            max_events: 800,
            max_item_bytes: 32 * 1024,
        }
    }
}

/// 呼び手に渡す履歴。
#[derive(Debug, Clone, PartialEq)]
pub struct HistorySnapshot {
    pub events: Vec<ConversationEvent>,
    /// 今 turn の最中か（`ReplayEnd` にそのまま載せる）。
    pub in_flight: bool,
    /// 上限を超えて、古い方を落としたか・1 件を切ったか。
    pub truncated: bool,
}

#[derive(Debug, Default)]
struct Turn {
    events: Vec<ConversationEvent>,
    bytes: usize,
}

#[derive(Debug)]
pub struct History {
    limits: HistoryLimits,
    turns: VecDeque<Turn>,
    bytes: usize,
    events: usize,
    truncated: bool,
}

impl History {
    pub fn new(limits: HistoryLimits) -> Self {
        Self {
            limits,
            turns: VecDeque::new(),
            bytes: 0,
            events: 0,
            truncated: false,
        }
    }

    /// event を 1 件入れる。会話の中身でないもの（SessionInit / TurnCompleted 等）は入れない。
    pub fn push(&mut self, mut event: ConversationEvent) {
        use ConversationEvent as E;
        let max = self.limits.max_item_bytes;
        if self.merge_chunk(&event) {
            return;
        }
        match &mut event {
            E::MessageChunk { text } | E::ThoughtChunk { text } => {
                self.truncated |= trim_text(text, max)
            }
            E::UserMessage { text } => self.truncated |= trim_text(text, max),
            E::ToolCallUpdate { content, .. } => self.truncated |= trim_text(content, max),
            E::ToolCall { input, .. } => {
                let mut text = input.to_string();
                if trim_text(&mut text, max) {
                    *input = serde_json::json!({ "excerpt": text });
                    self.truncated = true;
                }
            }
            E::Plan { .. } => {}
            _ => return,
        }
        if matches!(event, E::UserMessage { .. }) || self.turns.is_empty() {
            self.turns.push_back(Turn::default());
        }
        let bytes = serde_json::to_vec(&event)
            .expect("ConversationEvent は serialize できる")
            .len();
        let turn = self.turns.back_mut().expect("直前に足した");
        turn.events.push(event);
        turn.bytes += bytes;
        self.bytes += bytes;
        self.events += 1;
        self.enforce();
    }

    /// 続く chunk を、直前の同じ種類の 1 件に足す。足したら true。
    fn merge_chunk(&mut self, event: &ConversationEvent) -> bool {
        use ConversationEvent as E;
        let max = self.limits.max_item_bytes;
        let Some(turn) = self.turns.back_mut() else {
            return false;
        };
        let (kept, add) = match (turn.events.last_mut(), event) {
            (Some(E::MessageChunk { text: kept }), E::MessageChunk { text })
            | (Some(E::ThoughtChunk { text: kept }), E::ThoughtChunk { text }) => (kept, text),
            _ => return false,
        };
        let before = kept.len();
        kept.push_str(add);
        let cut = trim_text(kept, max);
        let after = kept.len();
        turn.bytes = turn.bytes + after - before;
        self.bytes = self.bytes + after - before;
        self.truncated |= cut;
        self.enforce();
        true
    }

    /// 上限に収まるまで古い方から落とす。turn が 2 つ以上あれば turn ごと、
    /// 1 つしか無ければその中の古い event から（最後の 1 件は残す）。
    fn enforce(&mut self) {
        while self.bytes > self.limits.max_bytes || self.events > self.limits.max_events {
            if self.turns.len() > 1 {
                let old = self.turns.pop_front().expect("2 つ以上ある");
                self.bytes -= old.bytes;
                self.events -= old.events.len();
            } else {
                let Some(turn) = self.turns.front_mut().filter(|t| t.events.len() > 1) else {
                    break;
                };
                let old = turn.events.remove(0);
                let bytes = serde_json::to_vec(&old)
                    .expect("ConversationEvent は serialize できる")
                    .len();
                turn.bytes -= bytes;
                self.bytes -= bytes;
                self.events -= 1;
            }
            self.truncated = true;
        }
    }

    pub fn snapshot(&self, in_flight: bool) -> HistorySnapshot {
        HistorySnapshot {
            events: self
                .turns
                .iter()
                .flat_map(|t| t.events.iter().cloned())
                .collect(),
            in_flight,
            truncated: self.truncated,
        }
    }
}

/// 上限を超えた text を、末尾を残して切る。切ったら true。
fn trim_text(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    *text = format!("…（省略）\n{}", &text[start..]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConversationEvent as E;

    fn user(text: &str) -> E {
        E::UserMessage { text: text.into() }
    }
    fn msg(text: &str) -> E {
        E::MessageChunk { text: text.into() }
    }
    fn thought(text: &str) -> E {
        E::ThoughtChunk { text: text.into() }
    }
    fn call(id: &str) -> E {
        E::ToolCall {
            id: id.into(),
            name: "tool".into(),
            input: serde_json::json!({}),
        }
    }
    fn result(id: &str, content: &str) -> E {
        E::ToolCallUpdate {
            tool_use_id: id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    #[test]
    fn keeps_order_and_merges_adjacent_chunks() {
        let mut h = History::new(HistoryLimits::default());
        for e in [
            user("hi"),
            thought("考え"),
            thought("中"),
            call("t1"),
            result("t1", "out"),
            msg("he"),
            msg("llo"),
            user("next"),
            msg("ok"),
        ] {
            h.push(e);
        }
        let snap = h.snapshot(true);
        assert_eq!(
            snap.events,
            vec![
                user("hi"),
                thought("考え中"),
                call("t1"),
                result("t1", "out"),
                msg("hello"),
                user("next"),
                msg("ok"),
            ]
        );
        assert!(snap.in_flight);
        assert!(!snap.truncated);
    }

    /// user の発話は chunk と違い、続いても 1 つにまとめない（別の発話）。
    #[test]
    fn adjacent_user_messages_stay_separate() {
        let mut h = History::new(HistoryLimits::default());
        h.push(user("a"));
        h.push(user("b"));
        assert_eq!(h.snapshot(false).events, vec![user("a"), user("b")]);
    }

    #[test]
    fn ignores_non_conversation_events() {
        let mut h = History::new(HistoryLimits::default());
        h.push(user("hi"));
        h.push(E::TurnCompleted {
            session_id: "s".into(),
            cost_usd: None,
            context_tokens: None,
            context_window: None,
        });
        h.push(E::Notice {
            message: "n".into(),
        });
        h.push(E::ReplayStart);
        assert_eq!(h.snapshot(false).events, vec![user("hi")]);
    }

    #[test]
    fn oversized_item_keeps_its_tail_and_marks_truncated() {
        let limits = HistoryLimits {
            max_item_bytes: 16,
            ..HistoryLimits::default()
        };
        let mut h = History::new(limits);
        h.push(user("q"));
        h.push(result("t1", &format!("{}END", "x".repeat(100))));
        h.push(E::ToolCall {
            id: "t2".into(),
            name: "write".into(),
            input: serde_json::json!({"Content": "y".repeat(100)}),
        });
        let snap = h.snapshot(false);
        assert!(snap.truncated);
        let E::ToolCallUpdate { content, .. } = &snap.events[1] else {
            panic!("tool の結果")
        };
        assert!(content.ends_with("END") && content.len() < 60, "{content}");
        let E::ToolCall { input, .. } = &snap.events[2] else {
            panic!("tool の呼び出し")
        };
        assert!(
            input.get("excerpt").is_some(),
            "大きい入力は抜粋に置き換える"
        );
    }

    /// 続く chunk をまとめた結果が上限を超えても、1 件の上限は守る。
    #[test]
    fn merged_chunks_respect_item_limit() {
        let limits = HistoryLimits {
            max_item_bytes: 16,
            ..HistoryLimits::default()
        };
        let mut h = History::new(limits);
        h.push(user("q"));
        for _ in 0..20 {
            h.push(msg("abcdefgh"));
        }
        let snap = h.snapshot(false);
        assert_eq!(snap.events.len(), 2);
        let E::MessageChunk { text } = &snap.events[1] else {
            panic!("本文")
        };
        assert!(text.len() < 60 && text.ends_with("abcdefgh"), "{text}");
        assert!(snap.truncated);
    }

    #[test]
    fn drops_oldest_whole_turn_when_over_event_limit() {
        let limits = HistoryLimits {
            max_events: 5,
            ..HistoryLimits::default()
        };
        let mut h = History::new(limits);
        for e in [user("1"), call("a"), result("a", "x"), user("2"), call("b")] {
            h.push(e);
        }
        assert!(!h.snapshot(false).truncated);
        h.push(result("b", "y"));
        let snap = h.snapshot(false);
        assert_eq!(
            snap.events,
            vec![user("2"), call("b"), result("b", "y")],
            "古い turn を丸ごと落とす（tool の開始と結果を切り離さない）"
        );
        assert!(snap.truncated);
    }

    #[test]
    fn drops_oldest_turns_when_over_byte_limit() {
        let limits = HistoryLimits {
            max_bytes: 400,
            ..HistoryLimits::default()
        };
        let mut h = History::new(limits);
        for i in 0..10 {
            h.push(user(&format!("q{i}")));
            h.push(msg(&"z".repeat(100)));
        }
        let snap = h.snapshot(false);
        assert!(snap.truncated);
        assert_eq!(snap.events.last(), Some(&msg(&"z".repeat(100))));
        assert_eq!(
            snap.events
                .first()
                .map(|e| matches!(e, E::UserMessage { .. })),
            Some(true)
        );
        let size: usize = snap
            .events
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap().len())
            .sum();
        assert!(size <= 400, "{size}");
        assert!(snap.events.len() >= 2);
    }

    /// 1 つの turn だけで上限を超える時は、その turn の古い event から落とす（空にはしない）。
    #[test]
    fn single_huge_turn_drops_its_oldest_events() {
        let limits = HistoryLimits {
            max_events: 3,
            ..HistoryLimits::default()
        };
        let mut h = History::new(limits);
        for e in [
            user("q"),
            call("a"),
            result("a", "1"),
            call("b"),
            result("b", "2"),
        ] {
            h.push(e);
        }
        let snap = h.snapshot(false);
        assert_eq!(
            snap.events,
            vec![result("a", "1"), call("b"), result("b", "2")]
        );
        assert!(snap.truncated);
    }

    /// user の発話より前に来た event（load の流れが途中から始まる等）も落とさない。
    #[test]
    fn events_before_first_user_message_are_kept() {
        let mut h = History::new(HistoryLimits::default());
        h.push(msg("a"));
        h.push(user("q"));
        assert_eq!(h.snapshot(false).events, vec![msg("a"), user("q")]);
    }
}
