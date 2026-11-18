//! ACP（Agent Client Protocol、protocol v1）の client host。設計は `docs/design/01-acp-client-host.md`。
//!
//! 純関数（calculations）: [`translate`]・[`config`]・[`session_meta`]・[`permission`]・[`history`]。
pub mod config;
pub mod history;
pub mod session_meta;
pub mod translate;
