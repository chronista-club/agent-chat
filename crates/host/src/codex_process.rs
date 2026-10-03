//! Native app-server launch contract shared by process hosts.
//!
//! CLI discovery, workspace identity and MCP credentials belong to the application.
//! This layer only selects the stdio protocol and preserves explicit config overrides.
use std::{ffi::OsStr, process::Stdio};
use tokio::process::Command;

/// Build an app-server command without selecting permissions or rewriting user config.
/// Callers can set their cwd and environment before spawning the returned command.
pub fn command(executable: &OsStr, config_overrides: &[String]) -> Command {
    let mut command = Command::new(executable);
    command.args(["app-server", "--listen", "stdio://"]);
    for value in config_overrides {
        command.arg("-c").arg(value);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_defaults_do_not_inject_permissions() {
        let cmd = command(OsStr::new("codex"), &[]);
        assert_eq!(
            cmd.as_std().get_args().collect::<Vec<_>>(),
            ["app-server", "--listen", "stdio://"]
        );
        assert_eq!(cmd.as_std().get_envs().count(), 0);
    }

    #[test]
    fn config_values_remain_single_arguments_in_order() {
        let overrides = vec![
            "mcp_servers.estimate.command=\"/workspace/tool path\"".to_string(),
            "developer_instructions=\"read the project's rules\"".to_string(),
        ];
        let cmd = command(OsStr::new("/Applications/Codex CLI/codex"), &overrides);
        assert_eq!(cmd.as_std().get_program(), "/Applications/Codex CLI/codex");
        assert_eq!(
            cmd.as_std().get_args().collect::<Vec<_>>(),
            [
                "app-server",
                "--listen",
                "stdio://",
                "-c",
                &overrides[0],
                "-c",
                &overrides[1]
            ]
        );
    }
}
