//! Claude / Codex の共有 wire 契約。アプリ・transport には依存しない。
pub mod codex_permissions;
pub mod codex_requests;
pub mod codex_settings;
pub mod event;
pub use event::*;

pub fn is_valid_model(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' || c == '/')
}
