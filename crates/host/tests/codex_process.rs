//! Launch contract at the actual JSONL process boundary; no model or credentials.
#![cfg(unix)]
use agent_chat_host::codex_process::command;
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn injected_identity_and_workspace_reach_the_stdio_process() {
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("codex fixture");
    std::fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$1\" \"$2\" \"$3\"\npwd\nprintf '%s\\n' \"$AGENT_CHAT_TEST_ID\"\nprintf 'diagnostic' >&2\ncat\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut cmd = command(executable.as_os_str(), &[]);
    cmd.current_dir(temp.path())
        .env("AGENT_CHAT_TEST_ID", "isolated-conversation");
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stderr = child.stderr.take().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        for expected in ["app-server", "--listen", "stdio://"] {
            assert_eq!(stdout.next_line().await.unwrap().as_deref(), Some(expected));
        }
        let cwd = stdout.next_line().await.unwrap().unwrap();
        assert_eq!(
            std::fs::canonicalize(cwd).unwrap(),
            std::fs::canonicalize(temp.path()).unwrap()
        );
        assert_eq!(
            stdout.next_line().await.unwrap().as_deref(),
            Some("isolated-conversation")
        );
        let frame = r#"{"id":1,"method":"initialize","params":{}}"#;
        stdin
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        assert_eq!(stdout.next_line().await.unwrap().as_deref(), Some(frame));
        drop(stdin);
        assert!(child.wait().await.unwrap().success());
        let mut diagnostic = String::new();
        stderr.read_to_string(&mut diagnostic).await.unwrap();
        assert_eq!(diagnostic, "diagnostic");
    })
    .await
    .expect("stdio roundtrip timed out");
}
