//! ACP（Agent Client Protocol、protocol v1）の client host。設計は `docs/design/01-acp-client-host.md`。
//!
//! - 純関数（calculations）: [`translate`]・[`config`]・[`session_meta`]・[`permission`]・[`history`]
//! - actions: [`host`]（process の起動、接続、命令のループ）
pub mod config;
pub mod history;
pub mod host;
pub mod permission;
pub mod session_meta;
pub mod translate;

pub use config::{AcpConfigChoice, AcpConfigOption, AcpConfigView};
pub use history::{HistoryLimits, HistorySnapshot};
pub use host::{
    AcpClientHost, Connector, HostError, HostOptions, LaunchContext, LoadFallback, PromptImage,
};
pub use permission::PermissionDecision;
