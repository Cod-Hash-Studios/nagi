use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::manifest::{effective_platforms, ensure_platform_supported};
use super::plugin_manifest_available;
use crate::api::schema::{
    InstalledPluginInfo, PluginCommandLogInfo, PluginCommandStatus, PluginInvocationContext,
};
use crate::app::App;

const PLUGIN_COMMAND_OUTPUT_MAX_BYTES: usize = 64 * 1024;
const PLUGIN_COMMAND_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(super) const MAX_PLUGIN_COMMANDS_IN_FLIGHT: usize = 32;
const PLUGIN_COMMAND_LOG_LIMIT: usize = 200;
const PLUGIN_INHERITED_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
];

impl App {
    pub(super) fn start_plugin_command(
        &mut self,
        plugin: &InstalledPluginInfo,
        action_id: Option<String>,
        event: Option<String>,
        command: Vec<String>,
        context: &PluginInvocationContext,
        event_json: Option<String>,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        if plugin.runtime == crate::api::schema::PluginRuntimeV2::WasiComponent {
            return self.start_sandbox_component(plugin, action_id, event, context, event_json);
        }
        if !plugin.native_trusted {
            return Err((
                "plugin_native_trust_required",
                format!(
                    "plugin {} has not been granted native trust",
                    plugin.plugin_id
                ),
            ));
        }
        let Some(program) = command.first().cloned() else {
            return Err((
                "invalid_plugin_command",
                "command must not be empty".to_string(),
            ));
        };
        let args = command.iter().skip(1).cloned().collect::<Vec<_>>();
        let context_json = serde_json::to_string(context)
            .map_err(|err| ("invalid_plugin_context", err.to_string()))?;
        super::env::ensure_plugin_user_dirs(plugin)
            .map_err(|err| ("plugin_user_dir_create_failed", err.to_string()))?;
        let log_id = format!("plugin-log-{}", self.state.next_plugin_command_log_id);
        self.state.next_plugin_command_log_id += 1;
        let started_unix_ms = current_unix_ms();
        let mut env = super::env::plugin_path_env(plugin);
        env.extend([
            (
                crate::api::SOCKET_PATH_ENV_VAR.to_string(),
                crate::api::socket_path().display().to_string(),
            ),
            ("NAGI_ENV".to_string(), "1".to_string()),
            ("NAGI_PLUGIN_ID".to_string(), plugin.plugin_id.clone()),
            ("NAGI_PLUGIN_CONTEXT_JSON".to_string(), context_json),
        ]);
        if let Ok(current_exe) = std::env::current_exe() {
            env.push((
                "NAGI_BIN_PATH".to_string(),
                current_exe.display().to_string(),
            ));
        }
        if let Some(action_id) = action_id.as_ref() {
            env.push(("NAGI_PLUGIN_ACTION_ID".to_string(), action_id.clone()));
        }
        if let Some(event) = event.as_ref() {
            env.push(("NAGI_PLUGIN_EVENT".to_string(), event.clone()));
        }
        if let Some(event_json) = event_json {
            env.push(("NAGI_PLUGIN_EVENT_JSON".to_string(), event_json));
        }
        if let Some(workspace_id) = context.workspace_id.as_ref() {
            env.push(("NAGI_WORKSPACE_ID".to_string(), workspace_id.clone()));
        }
        if let Some(tab_id) = context.tab_id.as_ref() {
            env.push(("NAGI_TAB_ID".to_string(), tab_id.clone()));
        }
        if let Some(pane_id) = context.focused_pane_id.as_ref() {
            env.push(("NAGI_PANE_ID".to_string(), pane_id.clone()));
        }
        if let Some(clicked_url) = context.clicked_url.as_ref() {
            env.push(("NAGI_PLUGIN_CLICKED_URL".to_string(), clicked_url.clone()));
        }
        if let Some(link_handler_id) = context.link_handler_id.as_ref() {
            env.push((
                "NAGI_PLUGIN_LINK_HANDLER_ID".to_string(),
                link_handler_id.clone(),
            ));
        }
        if self.state.plugin_commands_in_flight >= MAX_PLUGIN_COMMANDS_IN_FLIGHT {
            let message = format!(
                "maximum concurrent plugin commands reached ({MAX_PLUGIN_COMMANDS_IN_FLIGHT})"
            );
            let log = PluginCommandLogInfo {
                log_id,
                plugin_id: plugin.plugin_id.clone(),
                action_id,
                event,
                command,
                status: PluginCommandStatus::Failed,
                started_unix_ms,
                finished_unix_ms: Some(started_unix_ms),
                exit_code: None,
                stdout: Some(String::new()),
                stderr: Some(String::new()),
                error: Some(message.clone()),
            };
            self.push_plugin_command_log(log);
            return Err(("plugin_command_limit_reached", message));
        }
        let plugin_root = std::path::PathBuf::from(&plugin.plugin_root);
        let log = PluginCommandLogInfo {
            log_id: log_id.clone(),
            plugin_id: plugin.plugin_id.clone(),
            action_id,
            event,
            command: command.clone(),
            status: PluginCommandStatus::Running,
            started_unix_ms,
            finished_unix_ms: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
        };
        self.push_plugin_command_log(log.clone());
        self.state.plugin_commands_in_flight += 1;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result =
                execute_plugin_command(&program, &args, &plugin_root, env, PLUGIN_COMMAND_TIMEOUT);
            let finished = crate::events::AppEvent::PluginCommandFinished {
                log_id,
                finished_unix_ms: current_unix_ms(),
                exit_code: result.exit_code,
                stdout: result.stdout,
                stderr: result.stderr,
                error: result.error,
            };
            let _ = event_tx.blocking_send(finished);
        });
        Ok(log)
    }

    fn start_sandbox_component(
        &mut self,
        plugin: &InstalledPluginInfo,
        action_id: Option<String>,
        event: Option<String>,
        context: &PluginInvocationContext,
        event_json: Option<String>,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        super::ensure_plugin_capabilities_approved(plugin)?;
        let workspace_mount = sandbox_workspace_mount(&plugin.requested_capabilities, context)?;
        let entrypoint = plugin.entrypoint.as_ref().ok_or_else(|| {
            (
                "plugin_v2_entrypoint_missing",
                format!("plugin {} has no sandbox entrypoint", plugin.plugin_id),
            )
        })?;
        let context_json = serde_json::to_string(context)
            .map_err(|error| ("invalid_plugin_context", error.to_string()))?;
        if self.state.plugin_commands_in_flight >= MAX_PLUGIN_COMMANDS_IN_FLIGHT {
            return Err((
                "plugin_command_limit_reached",
                format!(
                    "maximum concurrent plugin commands reached ({MAX_PLUGIN_COMMANDS_IN_FLIGHT})"
                ),
            ));
        }
        let log_id = format!("plugin-log-{}", self.state.next_plugin_command_log_id);
        self.state.next_plugin_command_log_id += 1;
        let started_unix_ms = current_unix_ms();
        let command = vec!["wasi-component".to_owned(), entrypoint.clone()];
        let log = PluginCommandLogInfo {
            log_id: log_id.clone(),
            plugin_id: plugin.plugin_id.clone(),
            action_id: action_id.clone(),
            event: event.clone(),
            command,
            status: PluginCommandStatus::Running,
            started_unix_ms,
            finished_unix_ms: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
        };
        self.push_plugin_command_log(log.clone());
        self.state.plugin_commands_in_flight += 1;
        let event_tx = self.event_tx.clone();
        let component_path = std::path::PathBuf::from(entrypoint);
        let plugin_id = plugin.plugin_id.clone();
        let stdin = event_json.unwrap_or_default().into_bytes();
        std::thread::spawn(move || {
            let execution = super::sandbox::execute(
                super::sandbox::SandboxInvocation {
                    component_path: &component_path,
                    plugin_id: &plugin_id,
                    action_id: action_id.as_deref(),
                    context_json: &context_json,
                    stdin: &stdin,
                    workspace: workspace_mount.as_ref().map(|mount| {
                        super::sandbox::SandboxWorkspaceMount {
                            host_path: &mount.path,
                            readable: mount.readable,
                            writable: mount.writable,
                        }
                    }),
                },
                super::sandbox::SANDBOX_TIMEOUT,
            );
            let finished = crate::events::AppEvent::PluginCommandFinished {
                log_id,
                finished_unix_ms: current_unix_ms(),
                exit_code: execution.exit_code,
                stdout: execution.stdout,
                stderr: execution.stderr,
                error: execution.error,
            };
            let _ = event_tx.blocking_send(finished);
        });
        Ok(log)
    }

    pub(crate) fn run_plugin_event_hooks(&mut self, event: &crate::api::schema::EventEnvelope) {
        let event_name = event.event.dot_name();
        if !crate::api::schema::PLUGIN_HOOK_EVENT_KINDS.contains(&event.event) {
            return;
        }
        let plugins = self
            .state
            .installed_plugins
            .values()
            .filter(|plugin| {
                plugin.enabled
                    && plugin_manifest_available(plugin)
                    && plugin.events.iter().any(|hook| hook.on == event_name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if plugins.is_empty() {
            return;
        }
        let event_json = serde_json::to_string(event).ok();
        let context = self.plugin_context_for_event(event, event_name);
        for plugin in plugins {
            for hook in plugin.events.clone() {
                if hook.on != event_name {
                    continue;
                }
                if ensure_platform_supported(
                    &effective_platforms(&hook.platforms, &plugin.platforms).clone(),
                    event_name,
                )
                .is_err()
                {
                    continue;
                }
                let _ = self.start_plugin_command(
                    &plugin,
                    None,
                    Some(event_name.to_string()),
                    hook.command.clone(),
                    &context,
                    event_json.clone(),
                );
            }
        }
    }

    fn push_plugin_command_log(&mut self, log: PluginCommandLogInfo) {
        self.state.plugin_command_logs.push(log);
        if self.state.plugin_command_logs.len() > PLUGIN_COMMAND_LOG_LIMIT {
            let extra = self.state.plugin_command_logs.len() - PLUGIN_COMMAND_LOG_LIMIT;
            self.state.plugin_command_logs.drain(0..extra);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SandboxWorkspaceAccess {
    path: std::path::PathBuf,
    readable: bool,
    writable: bool,
}

fn sandbox_workspace_mount(
    requested_capabilities: &[String],
    context: &PluginInvocationContext,
) -> Result<Option<SandboxWorkspaceAccess>, (&'static str, String)> {
    use crate::plugin_capabilities::{PluginCapability, WorkspaceScope};

    crate::plugin_capabilities::ensure_runtime_bindings_available(requested_capabilities)
        .map_err(|error| ("plugin_capability_unavailable", error.to_string()))?;
    let capabilities = crate::plugin_capabilities::normalize_capabilities(requested_capabilities)
        .map_err(|error| ("invalid_plugin_capability", error.to_string()))?;
    let mut readable = false;
    let mut writable = false;
    for raw in capabilities {
        match PluginCapability::parse(&raw)
            .map_err(|error| ("invalid_plugin_capability", error.to_string()))?
        {
            PluginCapability::WorkspaceFilesRead(WorkspaceScope::Worktree) => readable = true,
            PluginCapability::WorkspaceFilesWrite(WorkspaceScope::Worktree) => writable = true,
            PluginCapability::MissionRead => {}
            PluginCapability::WorkspaceFilesRead(WorkspaceScope::Changed)
            | PluginCapability::WorkspaceFilesWrite(WorkspaceScope::Changed) => {
                unreachable!("unavailable changed-file capability passed host binding gate")
            }
            _ => unreachable!("unavailable capability passed host binding gate"),
        }
    }
    if !readable && !writable {
        return Ok(None);
    }
    let raw_path = context
        .worktree
        .as_ref()
        .map(|worktree| worktree.checkout_path.as_str())
        .or(context.workspace_cwd.as_deref())
        .ok_or_else(|| {
            (
                "plugin_workspace_unavailable",
                "this plugin action requires a workspace context".into(),
            )
        })?;
    let path = std::fs::canonicalize(raw_path).map_err(|error| {
        (
            "plugin_workspace_unavailable",
            format!("plugin workspace is unavailable: {error}"),
        )
    })?;
    if !path.is_dir() {
        return Err((
            "plugin_workspace_unavailable",
            "plugin workspace is not a directory".into(),
        ));
    }
    Ok(Some(SandboxWorkspaceAccess {
        path,
        readable,
        writable,
    }))
}

fn isolate_plugin_environment(command: &mut Command, env: Vec<(String, String)>) {
    command.env_clear();
    for key in PLUGIN_INHERITED_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.envs(env);
}

#[derive(Debug)]
struct PluginCommandExecution {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    error: Option<String>,
}

fn execute_plugin_command(
    program: &str,
    args: &[String],
    plugin_root: &std::path::Path,
    env: Vec<(String, String)>,
    timeout: Duration,
) -> PluginCommandExecution {
    let mut command = crate::plugin_command::command_for_argv(program, args);
    isolate_plugin_environment(&mut command, env);
    command
        .current_dir(plugin_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return PluginCommandExecution {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(error.to_string()),
            };
        }
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        terminate_plugin_process_group(&mut child);
        let _ = child.wait();
        return PluginCommandExecution {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("plugin command output pipe is unavailable".to_string()),
        };
    };
    let stdout_reader = std::thread::spawn(move || {
        read_capped_plugin_output(stdout, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
    });
    let stderr_reader = std::thread::spawn(move || {
        read_capped_plugin_output(stderr, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
    });

    let deadline = std::time::Instant::now() + timeout;
    let (exit_code, error) = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                terminate_plugin_process_group(&mut child);
                break (status.code(), None);
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Ok(None) => {
                terminate_plugin_process_group(&mut child);
                let _ = child.wait();
                break (
                    None,
                    Some(format!(
                        "plugin command timed out after {}ms",
                        timeout.as_millis()
                    )),
                );
            }
            Err(_) => {
                terminate_plugin_process_group(&mut child);
                let _ = child.wait();
                break (None, Some("plugin command wait failed".to_string()));
            }
        }
    };

    PluginCommandExecution {
        exit_code,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
        error,
    }
}

#[cfg(unix)]
fn terminate_plugin_process_group(child: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: the plugin process is spawned into a fresh process group
        // whose id is its child pid, and the negative id targets that group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn terminate_plugin_process_group(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let taskkill = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .map(|root| root.join("System32").join("taskkill.exe"))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| std::path::PathBuf::from("taskkill.exe"));
        let _ = Command::new(taskkill)
            .args(["/PID", &pid, "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(super) fn read_capped_plugin_output(mut reader: impl Read, cap: usize) -> String {
    let mut kept = Vec::with_capacity(cap.min(8192));
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = cap.saturating_sub(kept.len());
                if remaining > 0 {
                    kept.extend_from_slice(&buf[..n.min(remaining)]);
                }
                if n > remaining {
                    truncated = true;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let mut output = String::from_utf8_lossy(&kept).into_owned();
    if truncated {
        output.push_str(&format!(
            "\n[nagi truncated plugin output after {cap} bytes]"
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn timed_out_plugin_command_terminates_its_process_group() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("descendant-survived");
        let script = format!(
            "(sleep 0.25; printf survived > '{}') & wait",
            marker.display()
        );
        let started = std::time::Instant::now();

        let result = execute_plugin_command(
            "/bin/sh",
            &["-c".to_string(), script],
            root.path(),
            Vec::new(),
            std::time::Duration::from_millis(50),
        );

        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("timed out")),
            "unexpected plugin result: {result:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::thread::sleep(std::time::Duration::from_millis(350));
        assert!(
            !marker.exists(),
            "timed-out plugin descendant escaped process cleanup"
        );
    }

    #[test]
    fn sandbox_workspace_mount_is_exact_and_rejects_unbound_capabilities() {
        let workspace = tempfile::tempdir().unwrap();
        let context: PluginInvocationContext = serde_json::from_value(serde_json::json!({
            "workspace_cwd": workspace.path().to_string_lossy()
        }))
        .unwrap();
        let mount = sandbox_workspace_mount(
            &[
                "workspace.files.read:worktree".into(),
                "workspace.files.write:worktree".into(),
            ],
            &context,
        )
        .unwrap()
        .unwrap();
        assert_eq!(mount.path, workspace.path().canonicalize().unwrap());
        assert!(mount.readable);
        assert!(mount.writable);

        let changed = sandbox_workspace_mount(&["workspace.files.read:changed".into()], &context)
            .unwrap_err();
        assert_eq!(changed.0, "plugin_capability_unavailable");

        let network =
            sandbox_workspace_mount(&["network:https://example.com".into()], &context).unwrap_err();
        assert_eq!(network.0, "plugin_capability_unavailable");
    }
}
