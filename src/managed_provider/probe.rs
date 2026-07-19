use std::{path::Path, process::Stdio, time::Duration};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, Command},
};

use crate::mission::model::ProviderKind;

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_FRAMES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProtocolProbeError {
    #[error("protocol probe is not available for this provider")]
    UnsupportedProvider,
    #[error("provider process could not start: {0}")]
    Spawn(String),
    #[error("provider protocol handshake timed out")]
    Timeout,
    #[error("provider closed before acknowledging the protocol handshake")]
    Disconnected,
    #[error("provider returned an invalid protocol handshake")]
    Protocol,
}

pub(crate) async fn probe_protocol(
    provider: ProviderKind,
    executable: &Path,
    cwd: &Path,
    timeout: Duration,
) -> Result<(), ProtocolProbeError> {
    let (arguments, request) = match provider {
        ProviderKind::Codex => (
            vec![
                "app-server".to_owned(),
                "--listen".to_owned(),
                "stdio://".to_owned(),
            ],
            json!({
                "id": 1,
                "method": "initialize",
                "params": super::codex::initialize_params(),
            }),
        ),
        ProviderKind::ClaudeCode => {
            let mut arguments =
                super::claude::command_arguments(super::SandboxAccess::ReadOnly, None);
            arguments.push("--no-session-persistence".to_owned());
            (
                arguments,
                json!({
                    "type": "control_request",
                    "request_id": "nagi-probe",
                    "request": {"subtype": "initialize"},
                }),
            )
        }
        ProviderKind::OpenCode | ProviderKind::Acp => {
            return Err(ProtocolProbeError::UnsupportedProvider);
        }
    };

    let mut child = Command::new(executable)
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ProtocolProbeError::Spawn(error.to_string()))?;

    let result = run_handshake(provider, &mut child, request, timeout).await;
    stop_child(&mut child).await;
    result
}

async fn run_handshake(
    provider: ProviderKind,
    child: &mut Child,
    request: Value,
    timeout: Duration,
) -> Result<(), ProtocolProbeError> {
    let mut stdin = child.stdin.take().ok_or(ProtocolProbeError::Protocol)?;
    let stdout = child.stdout.take().ok_or(ProtocolProbeError::Protocol)?;
    if let Some(mut stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut sink = tokio::io::sink();
            let _ = tokio::io::copy(&mut stderr, &mut sink).await;
        });
    }

    let mut frame = serde_json::to_vec(&request).map_err(|_| ProtocolProbeError::Protocol)?;
    frame.push(b'\n');
    stdin
        .write_all(&frame)
        .await
        .map_err(|_| ProtocolProbeError::Disconnected)?;
    stdin
        .flush()
        .await
        .map_err(|_| ProtocolProbeError::Disconnected)?;

    let mut reader = BufReader::new(stdout);
    tokio::time::timeout(timeout, async {
        for _ in 0..MAX_FRAMES {
            let frame = read_bounded_line(&mut reader).await?;
            let value: Value =
                serde_json::from_slice(&frame).map_err(|_| ProtocolProbeError::Protocol)?;
            if handshake_succeeded(provider, &value) {
                return Ok(());
            }
            if handshake_failed(provider, &value) {
                return Err(ProtocolProbeError::Protocol);
            }
        }
        Err(ProtocolProbeError::Protocol)
    })
    .await
    .map_err(|_| ProtocolProbeError::Timeout)?
}

fn handshake_succeeded(provider: ProviderKind, value: &Value) -> bool {
    match provider {
        ProviderKind::Codex => {
            value.get("id") == Some(&json!(1))
                && value.get("result").is_some()
                && value.get("error").is_none()
        }
        ProviderKind::ClaudeCode => {
            value.get("type").and_then(Value::as_str) == Some("control_response")
                && value.pointer("/response/subtype").and_then(Value::as_str) == Some("success")
                && value
                    .pointer("/response/request_id")
                    .and_then(Value::as_str)
                    == Some("nagi-probe")
        }
        ProviderKind::OpenCode | ProviderKind::Acp => false,
    }
}

fn handshake_failed(provider: ProviderKind, value: &Value) -> bool {
    match provider {
        ProviderKind::Codex => {
            value.get("id") == Some(&json!(1)) && !handshake_succeeded(provider, value)
        }
        ProviderKind::ClaudeCode => {
            value.get("type").and_then(Value::as_str) == Some("control_response")
                && value
                    .pointer("/response/request_id")
                    .and_then(Value::as_str)
                    == Some("nagi-probe")
                && value.pointer("/response/subtype").and_then(Value::as_str) != Some("success")
        }
        ProviderKind::OpenCode | ProviderKind::Acp => true,
    }
}

async fn read_bounded_line(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<Vec<u8>, ProtocolProbeError> {
    let mut frame = Vec::new();
    let read = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut frame)
        .await
        .map_err(|_| ProtocolProbeError::Protocol)?;
    if read == 0 {
        return Err(ProtocolProbeError::Disconnected);
    }
    if frame.len() > MAX_FRAME_BYTES || frame.last() != Some(&b'\n') {
        return Err(ProtocolProbeError::Protocol);
    }
    Ok(frame)
}

async fn stop_child(child: &mut Child) {
    child.stdin.take();
    child.stdout.take();
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);

    fn fixture(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let executable = directory.join(name);
        std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        executable
    }

    #[tokio::test]
    async fn codex_probe_stops_after_initialize_without_starting_a_thread_or_turn() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("codex.log");
        let executable = fixture(
            directory.path(),
            "codex",
            &format!(
                "IFS= read -r line\nprintf '%s\\n' \"$line\" > '{}'\nprintf '%s\\n' '{{\"id\":1,\"result\":{{}}}}'\nwhile :; do sleep 1; done",
                log.display()
            ),
        );

        let result = probe_protocol(
            ProviderKind::Codex,
            &executable,
            directory.path(),
            FIXTURE_TIMEOUT,
        )
        .await;
        result.unwrap();

        let sent = std::fs::read_to_string(log).unwrap();
        assert!(sent.contains("\"method\":\"initialize\""));
        assert!(!sent.contains("thread/start"));
        assert!(!sent.contains("turn/start"));
    }

    #[tokio::test]
    async fn claude_probe_sends_no_user_message() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("claude.log");
        let executable = fixture(
            directory.path(),
            "claude",
            &format!(
                "IFS= read -r line\nprintf '%s\\n' \"$line\" > '{}'\nprintf '%s\\n' '{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"nagi-probe\",\"response\":{{}}}}}}'\nwhile :; do sleep 1; done",
                log.display()
            ),
        );

        probe_protocol(
            ProviderKind::ClaudeCode,
            &executable,
            directory.path(),
            FIXTURE_TIMEOUT,
        )
        .await
        .unwrap();

        let sent = std::fs::read_to_string(log).unwrap();
        assert!(sent.contains("\"subtype\":\"initialize\""));
        assert!(!sent.contains("\"type\":\"user\""));
        assert!(!sent.contains("prompt"));
    }

    #[test]
    fn malformed_matching_handshake_fails_closed_without_waiting_for_eof() {
        let response = json!({"id": 1, "result_typo": {}});
        assert!(!handshake_succeeded(ProviderKind::Codex, &response));
        assert!(handshake_failed(ProviderKind::Codex, &response));
    }
}
