use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::mpsc,
};

use super::{
    AttentionClass, ProviderAttention, ProviderCapabilities, ProviderCommand, ProviderEvent,
    ProviderResponse, ResponseToken, SandboxAccess, StartOrResume, TransportFailure, TurnOutcome,
};

const ACP_PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AcpEndpoint {
    Stdio {
        executable: PathBuf,
        args: Vec<String>,
    },
}

impl AcpEndpoint {
    pub(crate) fn stdio(
        executable: impl Into<PathBuf>,
        args: Vec<String>,
    ) -> Result<Self, AcpProtocolError> {
        let executable = executable.into();
        if executable.as_os_str().is_empty()
            || args
                .iter()
                .any(|arg| arg.is_empty() || arg.len() > 16 * 1024)
            || args.len() > 256
        {
            return Err(AcpProtocolError::InvalidEndpoint);
        }
        Ok(Self::Stdio { executable, args })
    }

    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "remote ACP is an explicit diagnostic boundary")
    )]
    pub(crate) fn parse_remote(_url: &str) -> Result<Self, AcpProtocolError> {
        Err(AcpProtocolError::RemoteTransportUnsupported)
    }
}

pub(super) fn spawn(
    endpoint: AcpEndpoint,
    commands: mpsc::Receiver<ProviderCommand>,
    events: mpsc::Sender<ProviderEvent>,
) {
    tokio::spawn(async move {
        if let Err(failure) = AcpActor::run(endpoint, commands, events.clone()).await {
            let _ = events
                .send(ProviderEvent::TransportFailed {
                    run_id: failure.run_id,
                    reason: failure.reason,
                })
                .await;
        }
    });
}

pub(super) fn spawn_from_executable(
    executable: Option<PathBuf>,
    commands: mpsc::Receiver<ProviderCommand>,
    events: mpsc::Sender<ProviderEvent>,
) {
    let Some(executable) = executable else {
        tokio::spawn(async move {
            let _ = events
                .send(ProviderEvent::TransportFailed {
                    run_id: "acp-unconfigured".into(),
                    reason: TransportFailure::Spawn,
                })
                .await;
        });
        return;
    };
    match AcpEndpoint::stdio(executable, Vec::new()) {
        Ok(endpoint) => spawn(endpoint, commands, events),
        Err(_) => {
            tokio::spawn(async move {
                let _ = events
                    .send(ProviderEvent::TransportFailed {
                        run_id: "acp-invalid-endpoint".into(),
                        reason: TransportFailure::Spawn,
                    })
                    .await;
            });
        }
    }
}

struct AcpActor {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    protocol: AcpProtocol,
    run_id: String,
    session_id: String,
    active_turn_id: Option<String>,
    next_turn: u64,
    pending_permissions: HashMap<String, PendingPermission>,
    events: mpsc::Sender<ProviderEvent>,
}

struct PendingPermission {
    request_id: Value,
    options: Vec<AcpPermissionOption>,
}

struct ActorFailure {
    run_id: String,
    reason: TransportFailure,
}

impl AcpActor {
    async fn run(
        endpoint: AcpEndpoint,
        mut commands: mpsc::Receiver<ProviderCommand>,
        events: mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ActorFailure> {
        let start = loop {
            match commands.recv().await {
                Some(ProviderCommand::StartOrResume(start)) => break start,
                Some(ProviderCommand::Shutdown) | None => return Ok(()),
                Some(_) => continue,
            }
        };
        let run_id = start.run_id.clone();
        let mut actor = Self::connect(endpoint, run_id.clone(), events)
            .await
            .map_err(|reason| ActorFailure {
                run_id: run_id.clone(),
                reason,
            })?;
        actor.initialize(&start).await.map_err(|_| ActorFailure {
            run_id: run_id.clone(),
            reason: TransportFailure::Protocol,
        })?;
        if !start.initial_input.trim().is_empty() {
            actor
                .start_turn(&start.initial_input)
                .await
                .map_err(|_| ActorFailure {
                    run_id: run_id.clone(),
                    reason: TransportFailure::Protocol,
                })?;
        }
        actor
            .event_loop(&mut commands)
            .await
            .map_err(|reason| ActorFailure { run_id, reason })
    }

    async fn connect(
        endpoint: AcpEndpoint,
        run_id: String,
        events: mpsc::Sender<ProviderEvent>,
    ) -> Result<Self, TransportFailure> {
        let AcpEndpoint::Stdio { executable, args } = endpoint;
        let mut child = Command::new(executable)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| TransportFailure::Spawn)?;
        let stdin = child.stdin.take().ok_or(TransportFailure::Spawn)?;
        let stdout = child.stdout.take().ok_or(TransportFailure::Spawn)?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            protocol: AcpProtocol::default(),
            run_id,
            session_id: String::new(),
            active_turn_id: None,
            next_turn: 1,
            pending_permissions: HashMap::new(),
            events,
        })
    }

    async fn initialize(&mut self, start: &StartOrResume) -> Result<(), AcpProtocolError> {
        if start.sandbox != SandboxAccess::WorkspaceWriteConfirmed {
            return Err(AcpProtocolError::WriteConsentRequired);
        }
        let initialize = self.protocol.initialize()?;
        self.write_frame(&initialize).await?;
        let response = self.read_frame_timeout().await?;
        let capabilities = self.protocol.accept_initialize_response(&response)?;
        if capabilities.auth_required {
            return Err(AcpProtocolError::AuthenticationRequired);
        }
        let session = if let Some(session_id) = start.resume_session_id.as_deref() {
            self.protocol.resume_session(session_id, &start.cwd)?
        } else {
            self.protocol.new_session(&start.cwd)?
        };
        let expected_id = session
            .get("id")
            .and_then(Value::as_u64)
            .ok_or(AcpProtocolError::MalformedFrame)?;
        self.write_frame(&session).await?;
        let response = self.read_response(expected_id).await?;
        self.session_id = if let Some(session_id) = start.resume_session_id.as_deref() {
            session_id.to_owned()
        } else {
            required_id(response.pointer("/result/sessionId"))?
        };
        self.events
            .send(ProviderEvent::Ready {
                run_id: self.run_id.clone(),
                session_id: self.session_id.clone(),
            })
            .await
            .map_err(|_| AcpProtocolError::TransportClosed)?;
        Ok(())
    }

    async fn event_loop(
        &mut self,
        commands: &mut mpsc::Receiver<ProviderCommand>,
    ) -> Result<(), TransportFailure> {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(ProviderCommand::SendTurn { input }) => {
                        if self.active_turn_id.is_some() {
                            return Err(TransportFailure::CommandRejected);
                        }
                        self.start_turn(&input).await.map_err(|_| TransportFailure::Protocol)?;
                    }
                    Some(ProviderCommand::Respond { token, response }) => {
                        self.respond(token, response).await.map_err(|_| TransportFailure::Protocol)?;
                    }
                    Some(ProviderCommand::Interrupt) | Some(ProviderCommand::Quiesce) => {
                        if self.active_turn_id.is_some() {
                            let cancel = self.protocol.cancel(&self.session_id).map_err(|_| TransportFailure::Protocol)?;
                            self.write_frame(&cancel).await.map_err(|_| TransportFailure::Disconnected)?;
                        }
                    }
                    Some(ProviderCommand::Shutdown) | None => {
                        let _ = self.child.kill().await;
                        let _ = self.events.send(ProviderEvent::Stopped { run_id: self.run_id.clone() }).await;
                        return Ok(());
                    }
                    Some(ProviderCommand::StartOrResume(_)) => return Err(TransportFailure::CommandRejected),
                },
                frame = self.read_frame() => {
                    let frame = frame.map_err(|_| TransportFailure::Disconnected)?;
                    self.handle_inbound(&frame).await.map_err(|_| TransportFailure::Protocol)?;
                }
            }
        }
    }

    async fn start_turn(&mut self, input: &str) -> Result<(), AcpProtocolError> {
        let request = self.protocol.prompt(&self.session_id, input)?;
        let turn_id = format!("acp-turn-{}", self.next_turn);
        self.next_turn = self.next_turn.saturating_add(1);
        self.active_turn_id = Some(turn_id.clone());
        self.write_frame(&request).await?;
        self.events
            .send(ProviderEvent::Working {
                run_id: self.run_id.clone(),
                turn_id,
            })
            .await
            .map_err(|_| AcpProtocolError::TransportClosed)
    }

    async fn handle_inbound(&mut self, frame: &[u8]) -> Result<(), AcpProtocolError> {
        match decode_inbound(frame)? {
            AcpInbound::TextDelta { session_id, text } => {
                if session_id != self.session_id {
                    return Err(AcpProtocolError::UnexpectedResponse);
                }
                self.events
                    .send(ProviderEvent::OutputDelta {
                        run_id: self.run_id.clone(),
                        turn_id: self
                            .active_turn_id
                            .clone()
                            .unwrap_or_else(|| "acp-idle".into()),
                        text,
                    })
                    .await
                    .map_err(|_| AcpProtocolError::TransportClosed)?;
            }
            AcpInbound::PermissionRequest {
                request_id,
                session_id,
                title,
                options,
            } => {
                if session_id != self.session_id {
                    return Err(AcpProtocolError::UnexpectedResponse);
                }
                let token = ResponseToken::for_external(&request_id, "session/request_permission")
                    .ok_or(AcpProtocolError::MalformedFrame)?;
                let audit_id = token.request_id().to_owned();
                self.pending_permissions.insert(
                    audit_id.clone(),
                    PendingPermission {
                        request_id,
                        options,
                    },
                );
                self.events
                    .send(ProviderEvent::AttentionRequested {
                        run_id: self.run_id.clone(),
                        attention: ProviderAttention {
                            token,
                            class: AttentionClass::PermissionApproval,
                            thread_id: self.session_id.clone(),
                            turn_id: self
                                .active_turn_id
                                .clone()
                                .unwrap_or_else(|| "acp-idle".into()),
                            item_id: audit_id,
                            requested_action: title,
                            questions: Vec::new(),
                        },
                    })
                    .await
                    .map_err(|_| AcpProtocolError::TransportClosed)?;
            }
            AcpInbound::PromptCompleted { outcome, .. } => {
                let turn_id = self
                    .active_turn_id
                    .take()
                    .ok_or(AcpProtocolError::UnexpectedResponse)?;
                self.events
                    .send(ProviderEvent::TurnCompleted {
                        run_id: self.run_id.clone(),
                        turn_id,
                        outcome: match outcome {
                            AcpTurnOutcome::Completed => TurnOutcome::Completed,
                            AcpTurnOutcome::Interrupted => TurnOutcome::Interrupted,
                            AcpTurnOutcome::Failed => TurnOutcome::Failed,
                        },
                    })
                    .await
                    .map_err(|_| AcpProtocolError::TransportClosed)?;
            }
            AcpInbound::IgnoredNotification => {}
        }
        Ok(())
    }

    async fn respond(
        &mut self,
        token: ResponseToken,
        response: ProviderResponse,
    ) -> Result<(), AcpProtocolError> {
        let audit_id = token.request_id().to_owned();
        let pending = self
            .pending_permissions
            .remove(&audit_id)
            .ok_or(AcpProtocolError::UnexpectedResponse)?;
        let preferred = match response {
            ProviderResponse::Approve => {
                [AcpPermissionKind::AllowOnce, AcpPermissionKind::AllowAlways]
            }
            ProviderResponse::ApproveForSession => {
                [AcpPermissionKind::AllowAlways, AcpPermissionKind::AllowOnce]
            }
            ProviderResponse::Decline | ProviderResponse::Answers(_) => [
                AcpPermissionKind::RejectOnce,
                AcpPermissionKind::RejectAlways,
            ],
        };
        let selected = preferred.into_iter().find_map(|kind| {
            pending
                .options
                .iter()
                .find(|option| option.kind == kind)
                .map(|option| option.option_id.as_str())
        });
        let frame = permission_response(pending.request_id, selected)?;
        self.write_frame(&frame).await?;
        self.events
            .send(ProviderEvent::ResponseResolved {
                run_id: self.run_id.clone(),
                request_id: audit_id,
            })
            .await
            .map_err(|_| AcpProtocolError::TransportClosed)
    }

    async fn write_frame(&mut self, value: &Value) -> Result<(), AcpProtocolError> {
        let payload = serde_json::to_vec(value)?;
        if payload.len() > MAX_FRAME_BYTES {
            return Err(AcpProtocolError::FrameSize);
        }
        self.stdin
            .write_all(&payload)
            .await
            .map_err(|_| AcpProtocolError::TransportClosed)?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|_| AcpProtocolError::TransportClosed)?;
        self.stdin
            .flush()
            .await
            .map_err(|_| AcpProtocolError::TransportClosed)
    }

    async fn read_frame_timeout(&mut self) -> Result<Vec<u8>, AcpProtocolError> {
        tokio::time::timeout(Duration::from_secs(10), self.read_frame())
            .await
            .map_err(|_| AcpProtocolError::Timeout)?
    }

    async fn read_response(&mut self, expected_id: u64) -> Result<Value, AcpProtocolError> {
        loop {
            let frame = self.read_frame_timeout().await?;
            let value = decode_frame(&frame)?;
            if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
                if value.get("error").is_some() {
                    return Err(AcpProtocolError::UnexpectedResponse);
                }
                return Ok(value);
            }
            if value.get("method").is_some() {
                self.handle_inbound(&frame).await?;
                continue;
            }
            return Err(AcpProtocolError::UnexpectedResponse);
        }
    }

    async fn read_frame(&mut self) -> Result<Vec<u8>, AcpProtocolError> {
        let mut frame = Vec::new();
        let read = (&mut self.stdout)
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut frame)
            .await
            .map_err(|_| AcpProtocolError::TransportClosed)?;
        if read == 0 {
            return Err(AcpProtocolError::TransportClosed);
        }
        if frame.len() > MAX_FRAME_BYTES {
            return Err(AcpProtocolError::FrameSize);
        }
        while matches!(frame.last(), Some(b'\n' | b'\r')) {
            frame.pop();
        }
        if frame.is_empty() {
            return Err(AcpProtocolError::MalformedFrame);
        }
        Ok(frame)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NegotiationState {
    Fresh,
    InitializeSent,
    Ready,
}

#[derive(Clone, Debug)]
pub(crate) struct AcpProtocol {
    state: NegotiationState,
    next_request_id: u64,
    capabilities: Option<AcpNegotiatedCapabilities>,
}

impl Default for AcpProtocol {
    fn default() -> Self {
        Self {
            state: NegotiationState::Fresh,
            next_request_id: 1,
            capabilities: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcpNegotiatedCapabilities {
    pub(crate) agent_name: Option<String>,
    pub(crate) agent_version: Option<String>,
    pub(crate) resume: bool,
    pub(crate) close: bool,
    pub(crate) auth_required: bool,
}

impl AcpNegotiatedCapabilities {
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "capabilities are exposed through the provider registry until runtime introspection is public"
        )
    )]
    pub(crate) const fn provider_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            resume: self.resume,
            turns: true,
            interrupt: true,
            permission_attention: true,
            question_attention: false,
            streaming_output: true,
            usage: false,
            diffs: false,
        }
    }
}

impl AcpProtocol {
    pub(crate) fn initialize(&mut self) -> Result<Value, AcpProtocolError> {
        if self.state != NegotiationState::Fresh {
            return Err(AcpProtocolError::InvalidState);
        }
        self.state = NegotiationState::InitializeSent;
        Ok(self.request(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false
                },
                "clientInfo": {
                    "name": "nagi",
                    "title": "Nagi",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ))
    }

    pub(crate) fn accept_initialize_response(
        &mut self,
        frame: &[u8],
    ) -> Result<AcpNegotiatedCapabilities, AcpProtocolError> {
        if self.state != NegotiationState::InitializeSent {
            return Err(AcpProtocolError::InvalidState);
        }
        let value = decode_frame(frame)?;
        if value.get("id").and_then(Value::as_u64) != Some(1) || value.get("error").is_some() {
            return Err(AcpProtocolError::UnexpectedResponse);
        }
        let result = value
            .get("result")
            .and_then(Value::as_object)
            .ok_or(AcpProtocolError::MalformedFrame)?;
        if result.get("protocolVersion").and_then(Value::as_u64) != Some(ACP_PROTOCOL_VERSION) {
            return Err(AcpProtocolError::UnsupportedVersion);
        }
        let capabilities = result.get("agentCapabilities");
        let session = capabilities.and_then(|value| value.get("sessionCapabilities"));
        let info = result.get("agentInfo");
        let negotiated = AcpNegotiatedCapabilities {
            agent_name: bounded_string(info.and_then(|value| value.get("name")), 128)?,
            agent_version: bounded_string(info.and_then(|value| value.get("version")), 128)?,
            resume: capabilities
                .and_then(|value| value.get("loadSession"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || session.is_some_and(|value| value.get("resume").is_some()),
            close: session.is_some_and(|value| value.get("close").is_some()),
            auth_required: result
                .get("authMethods")
                .and_then(Value::as_array)
                .is_some_and(|methods| !methods.is_empty()),
        };
        self.state = NegotiationState::Ready;
        self.capabilities = Some(negotiated.clone());
        Ok(negotiated)
    }

    pub(crate) fn new_session(&mut self, cwd: &Path) -> Result<Value, AcpProtocolError> {
        self.ensure_ready()?;
        if !cwd.is_absolute() {
            return Err(AcpProtocolError::InvalidWorkspace);
        }
        Ok(self.request("session/new", json!({"cwd": cwd, "mcpServers": []})))
    }

    pub(crate) fn resume_session(
        &mut self,
        session_id: &str,
        cwd: &Path,
    ) -> Result<Value, AcpProtocolError> {
        self.ensure_ready()?;
        if !self
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.resume)
        {
            return Err(AcpProtocolError::CapabilityUnavailable("resume"));
        }
        validate_session(session_id, cwd)?;
        Ok(self.request(
            "session/resume",
            json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
        ))
    }

    pub(crate) fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
    ) -> Result<Value, AcpProtocolError> {
        self.ensure_ready()?;
        validate_id(session_id)?;
        if text.trim().is_empty() || text.len() > 1024 * 1024 {
            return Err(AcpProtocolError::InvalidPrompt);
        }
        Ok(self.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}]
            }),
        ))
    }

    pub(crate) fn cancel(&self, session_id: &str) -> Result<Value, AcpProtocolError> {
        self.ensure_ready()?;
        validate_id(session_id)?;
        Ok(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session_id}
        }))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn ensure_ready(&self) -> Result<(), AcpProtocolError> {
        if self.state == NegotiationState::Ready {
            Ok(())
        } else {
            Err(AcpProtocolError::InvalidState)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AcpInbound {
    TextDelta {
        session_id: String,
        text: String,
    },
    PermissionRequest {
        request_id: Value,
        session_id: String,
        title: String,
        options: Vec<AcpPermissionOption>,
    },
    PromptCompleted {
        request_id: Value,
        outcome: AcpTurnOutcome,
    },
    IgnoredNotification,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcpPermissionOption {
    pub(crate) option_id: String,
    pub(crate) name: String,
    pub(crate) kind: AcpPermissionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AcpPermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AcpTurnOutcome {
    Completed,
    Interrupted,
    Failed,
}

pub(crate) fn decode_inbound(frame: &[u8]) -> Result<AcpInbound, AcpProtocolError> {
    let value = decode_frame(frame)?;
    if value.get("method").and_then(Value::as_str) == Some("session/update") {
        let params = value
            .get("params")
            .ok_or(AcpProtocolError::MalformedFrame)?;
        let session_id = required_id(params.get("sessionId"))?;
        let update = params
            .get("update")
            .ok_or(AcpProtocolError::MalformedFrame)?;
        if update.get("sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk")
            && update.pointer("/content/type").and_then(Value::as_str) == Some("text")
        {
            return Ok(AcpInbound::TextDelta {
                session_id,
                text: required_string(update.pointer("/content/text"), 1024 * 1024)?,
            });
        }
        return Ok(AcpInbound::IgnoredNotification);
    }
    if value.get("method").and_then(Value::as_str) == Some("session/request_permission") {
        let request_id = value
            .get("id")
            .filter(|id| id.is_string() || id.is_u64() || id.is_i64())
            .cloned()
            .ok_or(AcpProtocolError::MalformedFrame)?;
        let params = value
            .get("params")
            .ok_or(AcpProtocolError::MalformedFrame)?;
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .ok_or(AcpProtocolError::MalformedFrame)?;
        if options.is_empty() || options.len() > 32 {
            return Err(AcpProtocolError::MalformedFrame);
        }
        let options = options
            .iter()
            .map(parse_permission_option)
            .collect::<Result<Vec<_>, _>>()?;
        let title = bounded_string(params.pointer("/toolCall/title"), 4_096)?
            .unwrap_or_else(|| "ACP tool permission".to_owned());
        return Ok(AcpInbound::PermissionRequest {
            request_id,
            session_id: required_id(params.get("sessionId"))?,
            title,
            options,
        });
    }
    if value.get("id").is_some() {
        let request_id = value.get("id").cloned().unwrap_or(Value::Null);
        if value.get("error").is_some() {
            return Ok(AcpInbound::PromptCompleted {
                request_id,
                outcome: AcpTurnOutcome::Failed,
            });
        }
        if let Some(reason) = value.pointer("/result/stopReason").and_then(Value::as_str) {
            let outcome = match reason {
                "end_turn" => AcpTurnOutcome::Completed,
                "cancelled" => AcpTurnOutcome::Interrupted,
                "max_tokens" | "max_turn_requests" | "refusal" => AcpTurnOutcome::Failed,
                _ => return Err(AcpProtocolError::MalformedFrame),
            };
            return Ok(AcpInbound::PromptCompleted {
                request_id,
                outcome,
            });
        }
    }
    Err(AcpProtocolError::UnexpectedResponse)
}

pub(crate) fn permission_response(
    request_id: Value,
    selected_option_id: Option<&str>,
) -> Result<Value, AcpProtocolError> {
    if !(request_id.is_string() || request_id.is_u64() || request_id.is_i64()) {
        return Err(AcpProtocolError::MalformedFrame);
    }
    let outcome = selected_option_id.map_or_else(
        || json!({"outcome": "cancelled"}),
        |option_id| json!({"outcome": "selected", "optionId": option_id}),
    );
    Ok(json!({"jsonrpc": "2.0", "id": request_id, "result": {"outcome": outcome}}))
}

fn decode_frame(frame: &[u8]) -> Result<Value, AcpProtocolError> {
    if frame.is_empty() || frame.len() > MAX_FRAME_BYTES {
        return Err(AcpProtocolError::FrameSize);
    }
    let value: Value =
        serde_json::from_slice(frame).map_err(|_| AcpProtocolError::MalformedFrame)?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !value.is_object() {
        return Err(AcpProtocolError::MalformedFrame);
    }
    Ok(value)
}

fn parse_permission_option(value: &Value) -> Result<AcpPermissionOption, AcpProtocolError> {
    let kind = match value.get("kind").and_then(Value::as_str) {
        Some("allow_once") => AcpPermissionKind::AllowOnce,
        Some("allow_always") => AcpPermissionKind::AllowAlways,
        Some("reject_once") => AcpPermissionKind::RejectOnce,
        Some("reject_always") => AcpPermissionKind::RejectAlways,
        _ => return Err(AcpProtocolError::MalformedFrame),
    };
    Ok(AcpPermissionOption {
        option_id: required_string(value.get("optionId"), 1024)?,
        name: required_string(value.get("name"), 1024)?,
        kind,
    })
}

fn validate_session(session_id: &str, cwd: &Path) -> Result<(), AcpProtocolError> {
    validate_id(session_id)?;
    if !cwd.is_absolute() {
        return Err(AcpProtocolError::InvalidWorkspace);
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), AcpProtocolError> {
    if value.trim().is_empty() || value.len() > 1024 || value.contains(['\n', '\r', '\0']) {
        Err(AcpProtocolError::InvalidSession)
    } else {
        Ok(())
    }
}

fn bounded_string(value: Option<&Value>, max: usize) -> Result<Option<String>, AcpProtocolError> {
    value
        .map(|value| required_string(Some(value), max))
        .transpose()
}

fn required_string(value: Option<&Value>, max: usize) -> Result<String, AcpProtocolError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= max)
        .map(str::to_owned)
        .ok_or(AcpProtocolError::MalformedFrame)
}

fn required_id(value: Option<&Value>) -> Result<String, AcpProtocolError> {
    let value = required_string(value, 1024)?;
    validate_id(&value)?;
    Ok(value)
}

#[derive(Debug, Error)]
pub(crate) enum AcpProtocolError {
    #[error("ACP transport endpoint is invalid")]
    InvalidEndpoint,
    #[error("remote ACP transport is not supported; use a local stdio agent")]
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "remote ACP is rejected before transport setup")
    )]
    RemoteTransportUnsupported,
    #[error("ACP protocol operation is invalid in the current state")]
    InvalidState,
    #[error("ACP frame is empty or exceeds the 1 MiB limit")]
    FrameSize,
    #[error("ACP frame is malformed")]
    MalformedFrame,
    #[error("ACP response does not match the pending request")]
    UnexpectedResponse,
    #[error("ACP protocol version is unsupported")]
    UnsupportedVersion,
    #[error("ACP capability {0} was not negotiated")]
    CapabilityUnavailable(&'static str),
    #[error("ACP workspace must be absolute")]
    InvalidWorkspace,
    #[error("ACP session identifier is invalid")]
    InvalidSession,
    #[error("ACP prompt is empty or too large")]
    InvalidPrompt,
    #[error("ACP agents require explicit workspace-write consent")]
    WriteConsentRequired,
    #[error("ACP agent requires authentication; authenticate it before using Nagi")]
    AuthenticationRequired,
    #[error("ACP transport closed")]
    TransportClosed,
    #[error("ACP operation timed out")]
    Timeout,
    #[error("ACP JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn negotiated() -> AcpProtocol {
        let mut protocol = AcpProtocol::default();
        assert_eq!(protocol.initialize().unwrap()["method"], "initialize");
        protocol
            .accept_initialize_response(
                br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"fixture","version":"1.0"},"agentCapabilities":{"loadSession":false,"sessionCapabilities":{"resume":{},"close":{}}},"authMethods":[]}}"#,
            )
            .unwrap();
        protocol
    }

    #[test]
    fn initialization_is_first_and_negotiates_exact_capabilities() {
        let mut protocol = AcpProtocol::default();
        assert!(matches!(
            protocol.prompt("session", "hello"),
            Err(AcpProtocolError::InvalidState)
        ));
        let request = protocol.initialize().unwrap();
        assert_eq!(request["params"]["protocolVersion"], 1);
        let capabilities = protocol
            .accept_initialize_response(
                br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"agent","version":"2"},"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"authMethods":[{"id":"login","name":"Login"}]}}"#,
            )
            .unwrap();
        assert!(capabilities.resume);
        assert!(!capabilities.close);
        assert!(capabilities.auth_required);
        assert!(capabilities.provider_capabilities().permission_attention);
    }

    #[test]
    fn unsupported_version_and_malformed_frames_fail_closed() {
        let mut protocol = AcpProtocol::default();
        protocol.initialize().unwrap();
        assert!(matches!(
            protocol.accept_initialize_response(
                br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2}}"#
            ),
            Err(AcpProtocolError::UnsupportedVersion)
        ));
        assert!(matches!(
            decode_inbound(b"not-json"),
            Err(AcpProtocolError::MalformedFrame)
        ));
        assert!(matches!(
            decode_inbound(&vec![b'x'; MAX_FRAME_BYTES + 1]),
            Err(AcpProtocolError::FrameSize)
        ));
    }

    #[test]
    fn session_requests_are_capability_gated_and_use_text_blocks() {
        let mut protocol = negotiated();
        let session = protocol.new_session(Path::new("/tmp/project")).unwrap();
        assert_eq!(session["method"], "session/new");
        let prompt = protocol.prompt("session-1", "Fix the tests").unwrap();
        assert_eq!(prompt["params"]["prompt"][0]["type"], "text");
        assert_eq!(
            protocol.cancel("session-1").unwrap()["method"],
            "session/cancel"
        );
    }

    #[test]
    fn output_permission_and_completion_frames_are_mapped() {
        let output = decode_inbound(
            br#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            output,
            AcpInbound::TextDelta {
                session_id: "s1".into(),
                text: "done".into()
            }
        );
        let permission = decode_inbound(
            br#"{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"t1","title":"Run tests"},"options":[{"optionId":"once","name":"Allow once","kind":"allow_once"}]}}"#,
        )
        .unwrap();
        let AcpInbound::PermissionRequest { options, .. } = permission else {
            panic!("expected permission request");
        };
        assert_eq!(options[0].kind, AcpPermissionKind::AllowOnce);
        let completion =
            decode_inbound(br#"{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}"#)
                .unwrap();
        assert!(matches!(
            completion,
            AcpInbound::PromptCompleted {
                outcome: AcpTurnOutcome::Completed,
                ..
            }
        ));
    }

    #[test]
    fn remote_transport_is_explicitly_unsupported() {
        assert!(matches!(
            AcpEndpoint::parse_remote("https://agent.example"),
            Err(AcpProtocolError::RemoteTransportUnsupported)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_actor_runs_a_complete_permission_gated_turn() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("acp-fixture");
        std::fs::write(
            &executable,
            include_str!("../../tests/fixtures/acp/agent.py"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (commands_tx, commands_rx) = mpsc::channel(16);
        spawn(
            AcpEndpoint::stdio(executable, Vec::new()).unwrap(),
            commands_rx,
            events_tx,
        );
        commands_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-acp".into(),
                cwd: std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap(),
                resume_session_id: None,
                initial_input: "Continue the mission".into(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .await
            .unwrap();

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), events_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            ProviderEvent::Ready { ref run_id, ref session_id }
                if run_id == "run-acp" && session_id == "acp-session-1"
        ));
        assert!(matches!(
            events_rx.recv().await.unwrap(),
            ProviderEvent::Working { ref run_id, .. } if run_id == "run-acp"
        ));
        assert!(matches!(
            events_rx.recv().await.unwrap(),
            ProviderEvent::OutputDelta { ref text, .. } if text == "fixture output"
        ));
        let attention = events_rx.recv().await.unwrap();
        let ProviderEvent::AttentionRequested { attention, .. } = attention else {
            panic!("expected ACP permission attention");
        };
        commands_tx
            .send(ProviderCommand::Respond {
                token: attention.token,
                response: ProviderResponse::Approve,
            })
            .await
            .unwrap();
        assert!(matches!(
            events_rx.recv().await.unwrap(),
            ProviderEvent::ResponseResolved { ref run_id, .. } if run_id == "run-acp"
        ));
        assert!(matches!(
            events_rx.recv().await.unwrap(),
            ProviderEvent::TurnCompleted {
                ref run_id,
                outcome: TurnOutcome::Completed,
                ..
            } if run_id == "run-acp"
        ));
        commands_tx.send(ProviderCommand::Shutdown).await.unwrap();
    }
}
