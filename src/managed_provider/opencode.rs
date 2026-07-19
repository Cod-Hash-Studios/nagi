use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::{json, Value};
use tokio::{
    io::AsyncReadExt as _,
    process::{Child, ChildStdout, Command},
    sync::mpsc,
};

use super::{
    AttentionClass, ProviderAttention, ProviderCommand, ProviderEvent, ProviderResponse,
    ResponseToken, RpcId, SandboxAccess, StartOrResume, TransportFailure, TurnOutcome,
};

pub(crate) const TESTED_VERSION: &str = "1.18.3";
const REQUIRED_ROUTES: [(&str, &str); 10] = [
    ("/event", "get"),
    ("/session", "post"),
    ("/session/status", "get"),
    ("/session/{sessionID}", "get"),
    ("/session/{sessionID}/message", "get"),
    ("/session/{sessionID}/prompt_async", "post"),
    ("/session/{sessionID}/abort", "post"),
    ("/permission", "get"),
    ("/permission/{requestID}/reply", "post"),
    ("/question", "get"),
];
const MAX_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROVIDER_FRAME_BYTES: usize = 1024 * 1024;
const MAX_VISIBLE_TEXT_BYTES: usize = 16 * 1024;
const MAX_TURN_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SESSION_PART_BYTES: usize = 32 * 1024 * 1024;
const MAX_TRACKED_PARTS: usize = 8 * 1024;
const MAX_TRACKED_MESSAGES: usize = 8 * 1024;
const MAX_TRACKED_EVENTS: usize = 8 * 1024;
const MAX_PENDING_PERMISSIONS: usize = 128;
const MAX_STARTUP_OUTPUT_BYTES: usize = 64 * 1024;
const STDERR_DRAIN_BUFFER_BYTES: usize = 4 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 1024;
const MAX_ATTENTION_TEXT_BYTES: usize = 4 * 1024;
const MAX_PERMISSION_PATTERNS: usize = 128;
const PERMISSION_METHOD: &str = "opencode/permission";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
#[cfg(test)]
const TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn spawn(
    executable: Option<PathBuf>,
    commands: mpsc::Receiver<ProviderCommand>,
    events: mpsc::Sender<ProviderEvent>,
) {
    tokio::spawn(async move {
        Actor::spawned(
            executable.unwrap_or_else(|| PathBuf::from("opencode")),
            events,
        )
        .run(commands)
        .await;
    });
}

fn command_arguments() -> [&'static str; 6] {
    [
        "serve",
        "--hostname",
        "127.0.0.1",
        "--port",
        "0",
        "--no-mdns",
    ]
}

fn generate_password() -> Result<String, ()> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn permission_override(sandbox: SandboxAccess) -> Option<&'static str> {
    match sandbox {
        SandboxAccess::ReadOnly => {
            Some(r#"{"edit":"deny","bash":"deny","external_directory":"deny"}"#)
        }
        SandboxAccess::WorkspaceWriteConfirmed => None,
    }
}

fn permission_response_value(response: &ProviderResponse) -> Option<&'static str> {
    match response {
        ProviderResponse::Approve => Some("once"),
        ProviderResponse::ApproveForSession => Some("always"),
        ProviderResponse::Decline => Some("reject"),
        ProviderResponse::Answers(_) => None,
    }
}

fn parse_listening_url(line: &[u8]) -> Option<String> {
    const PREFIX: &str = "opencode server listening on ";
    let line = std::str::from_utf8(line).ok()?.trim();
    let url = line.strip_prefix(PREFIX)?;
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.scheme() != "http"
        || parsed.host_str() != Some("127.0.0.1")
        || parsed.port() == Some(0)
        || parsed.port().is_none()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    Some(format!("http://127.0.0.1:{}", parsed.port()?))
}

fn validate_server_contract(health: &Value, doc: &Value) -> Result<(), ()> {
    if health.get("healthy") != Some(&Value::Bool(true))
        || health.get("version").and_then(Value::as_str) != Some(TESTED_VERSION)
    {
        return Err(());
    }
    for (path, method) in REQUIRED_ROUTES {
        if doc
            .get("paths")
            .and_then(|paths| paths.get(path))
            .and_then(|path| path.get(method))
            .is_none()
        {
            return Err(());
        }
    }
    Ok(())
}

#[cfg(test)]
fn spawn_for_test(
    base_url: String,
    username: String,
    password: String,
    commands: mpsc::Receiver<ProviderCommand>,
    events: mpsc::Sender<ProviderEvent>,
) {
    tokio::spawn(async move {
        Actor::connected(base_url, username, password, events)
            .run(commands)
            .await;
    });
}

struct Actor {
    executable: Option<PathBuf>,
    base_url: String,
    username: String,
    password: String,
    client: Client,
    events: mpsc::Sender<ProviderEvent>,
    sse_rx: Option<mpsc::Receiver<SseMessage>>,
    run_id: Option<String>,
    start: Option<StartOrResume>,
    session_id: Option<String>,
    current_turn: Option<CurrentTurn>,
    next_turn_id: u64,
    parts: BTreeMap<String, String>,
    parts_bytes: usize,
    part_messages: BTreeMap<String, String>,
    completed_parts: BTreeSet<String>,
    message_roles: BTreeMap<String, MessageRole>,
    seen_event_ids: BTreeSet<String>,
    seen_event_order: VecDeque<String>,
    pending_permissions: BTreeMap<String, PendingPermission>,
    follow_external_turns: bool,
    quiesced: bool,
    child: Option<Child>,
}

struct CurrentTurn {
    id: String,
    armed: bool,
    working_emitted: bool,
    interrupt_requested: bool,
    assistant_messages: BTreeSet<String>,
    output_bytes: usize,
    deadline: tokio::time::Instant,
}

struct PendingPermission {
    audit_id: String,
    responding: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionPhase {
    Idle,
    Busy,
    Retry,
}

struct ResumeHydration {
    phase: SessionPhase,
    latest_assistant: Option<String>,
    permissions: BTreeMap<String, ProviderAttention>,
    questions: BTreeMap<String, Value>,
}

#[derive(Clone, Copy)]
enum MessageRole {
    User,
    Assistant,
}

enum SseMessage {
    Event(Value),
    Closed,
    Invalid,
}

#[derive(Default)]
struct SseDecoder {
    frame: Vec<u8>,
}

impl SseDecoder {
    fn push_byte(&mut self, byte: u8) -> Result<Option<Value>, ()> {
        self.frame.push(byte);
        let separator_len = if self.frame.ends_with(b"\r\n\r\n") {
            Some(4)
        } else if self.frame.ends_with(b"\n\n") {
            Some(2)
        } else {
            None
        };
        if let Some(separator_len) = separator_len {
            let frame_len = self.frame.len().checked_sub(separator_len).ok_or(())?;
            if frame_len > MAX_PROVIDER_FRAME_BYTES {
                return Err(());
            }
            self.frame.truncate(frame_len);
            let event = if self.frame.is_empty() {
                None
            } else {
                parse_sse_frame(&self.frame)?
            };
            self.frame.clear();
            return Ok(event);
        }

        let possible_separator_bytes = pending_separator_prefix_len(&self.frame);
        if self
            .frame
            .len()
            .checked_sub(possible_separator_bytes)
            .ok_or(())?
            > MAX_PROVIDER_FRAME_BYTES
        {
            return Err(());
        }
        Ok(None)
    }

    #[cfg(test)]
    fn push_chunk(&mut self, chunk: &[u8]) -> Result<Vec<Value>, ()> {
        let mut events = Vec::new();
        for byte in chunk.iter().copied() {
            if let Some(value) = self.push_byte(byte)? {
                events.push(value);
            }
        }
        Ok(events)
    }

    fn finish(self) -> Result<(), ()> {
        if self.frame.is_empty() {
            Ok(())
        } else {
            Err(())
        }
    }
}

impl Actor {
    fn spawned(executable: PathBuf, events: mpsc::Sender<ProviderEvent>) -> Self {
        let mut actor = Self::connected(String::new(), String::new(), String::new(), events);
        actor.executable = Some(executable);
        actor
    }

    fn connected(
        base_url: String,
        username: String,
        password: String,
        events: mpsc::Sender<ProviderEvent>,
    ) -> Self {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(REQUEST_TIMEOUT)
            .build()
            .expect("static reqwest client configuration is valid");
        Self {
            executable: None,
            base_url,
            username,
            password,
            client,
            events,
            sse_rx: None,
            run_id: None,
            start: None,
            session_id: None,
            current_turn: None,
            next_turn_id: 1,
            parts: BTreeMap::new(),
            parts_bytes: 0,
            part_messages: BTreeMap::new(),
            completed_parts: BTreeSet::new(),
            message_roles: BTreeMap::new(),
            seen_event_ids: BTreeSet::new(),
            seen_event_order: VecDeque::new(),
            pending_permissions: BTreeMap::new(),
            follow_external_turns: false,
            quiesced: false,
            child: None,
        }
    }

    async fn run(mut self, mut commands: mpsc::Receiver<ProviderCommand>) {
        let Some(command) = commands.recv().await else {
            return;
        };
        let ProviderCommand::StartOrResume(start) = command else {
            self.fail(TransportFailure::CommandRejected).await;
            return;
        };
        self.run_id = Some(start.run_id.clone());
        self.start = Some(start);
        if self.executable.is_some() && self.start_process().await.is_err() {
            self.fail(TransportFailure::Spawn).await;
            return;
        }
        if let Err(reason) = self.initialize().await {
            self.fail(reason).await;
            return;
        }

        loop {
            // Permission approval is human-paced, so the turn idle deadline is suspended while
            // waiting. `register_permission` bounds this in-memory set independently.
            let deadline = if self.pending_permissions.is_empty() {
                self.current_turn.as_ref().map(|turn| turn.deadline)
            } else {
                None
            };
            let sse_rx = self.sse_rx.as_mut().expect("SSE is initialized");
            tokio::select! {
                () = sleep_until(deadline) => {
                    self.fail(TransportFailure::Timeout).await;
                    return;
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        return;
                    };
                    if self.handle_command(command).await {
                        return;
                    }
                }
                message = sse_rx.recv() => {
                    match message {
                        Some(SseMessage::Event(event)) => {
                            if let Err(failure) = self.handle_event(&event).await {
                                self.fail(failure.transport_failure()).await;
                                return;
                            }
                        }
                        Some(SseMessage::Closed) | None => {
                            self.fail(if self.current_turn.is_some()
                                || self.pending_permissions.values().any(|pending| pending.responding)
                            {
                                TransportFailure::DeliveryUnknown
                            } else {
                                TransportFailure::Disconnected
                            }).await;
                            return;
                        }
                        Some(SseMessage::Invalid) => {
                            self.fail(TransportFailure::Protocol).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn start_process(&mut self) -> Result<(), ()> {
        let executable = self.executable.take().ok_or(())?;
        let password = generate_password()?;
        let start = self.start.as_ref().ok_or(())?;
        let mut command = Command::new(executable);
        command
            .args(command_arguments())
            .current_dir(&start.cwd)
            .env("OPENCODE_SERVER_USERNAME", "opencode")
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(permission) = permission_override(start.sandbox) {
            command.env("OPENCODE_PERMISSION", permission);
        }
        let mut child = command.spawn().map_err(|_| ())?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_output(stderr));
        }
        let mut stdout = child.stdout.take().ok_or(())?;
        let base_url = tokio::time::timeout(STARTUP_TIMEOUT, read_listening_url(&mut stdout))
            .await
            .map_err(|_| ())??;
        tokio::spawn(drain_output(stdout));
        self.base_url = base_url;
        self.username = "opencode".to_owned();
        self.password = password;
        self.child = Some(child);
        Ok(())
    }

    async fn initialize(&mut self) -> Result<(), TransportFailure> {
        let health = self
            .get_json("/global/health", None)
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        let doc = self
            .get_json("/doc", None)
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        validate_server_contract(&health, &doc).map_err(|()| TransportFailure::Protocol)?;
        let start = self.start.as_ref().ok_or(TransportFailure::Protocol)?;
        let directory = start
            .cwd
            .to_str()
            .ok_or(TransportFailure::Protocol)?
            .to_owned();
        let initial_input = start.initial_input.clone();
        let resume_session_id = start.resume_session_id.clone();
        let is_resume = resume_session_id.is_some();
        self.follow_external_turns = is_resume;
        let session_id = match resume_session_id.as_deref() {
            Some(session_id) => {
                validate_identifier(session_id).map_err(|()| TransportFailure::Protocol)?;
                let session = self
                    .get_json(&format!("/session/{session_id}"), Some(&directory))
                    .await
                    .map_err(|()| TransportFailure::Protocol)?;
                if session.get("id").and_then(Value::as_str) != Some(session_id) {
                    return Err(TransportFailure::Protocol);
                }
                if session.get("directory").and_then(Value::as_str) != Some(directory.as_str()) {
                    return Err(TransportFailure::Protocol);
                }
                session_id.to_owned()
            }
            None => {
                let session = tokio::time::timeout(REQUEST_TIMEOUT, async {
                    let response = self
                        .request(Method::POST, "/session", Some(&directory))
                        .json(&json!({}))
                        .send()
                        .await
                        .map_err(|_| ())?;
                    response_json_bounded(response, MAX_HTTP_BODY_BYTES).await
                })
                .await
                .map_err(|_| TransportFailure::DeliveryUnknown)?
                .map_err(|()| TransportFailure::DeliveryUnknown)?;
                let session_id = session
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(TransportFailure::DeliveryUnknown)?;
                validate_identifier(session_id).map_err(|()| TransportFailure::DeliveryUnknown)?;
                session_id.to_owned()
            }
        };
        self.session_id = Some(session_id.clone());
        self.open_sse(&directory)
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        let mut hydration = if is_resume {
            Some(self.hydrate_resume(&directory).await?)
        } else {
            None
        };
        if let Some(hydration) = hydration.as_mut() {
            if hydration.phase != SessionPhase::Idle {
                let turn_id = format!("turn-{}", self.next_turn_id);
                self.next_turn_id += 1;
                let mut assistant_messages = BTreeSet::new();
                if let Some(message_id) = hydration.latest_assistant.clone() {
                    assistant_messages.insert(message_id);
                }
                self.current_turn = Some(CurrentTurn {
                    id: turn_id,
                    armed: true,
                    working_emitted: false,
                    interrupt_requested: false,
                    assistant_messages,
                    output_bytes: 0,
                    deadline: tokio::time::Instant::now() + TURN_IDLE_TIMEOUT,
                });
                let current_turn_id = self
                    .current_turn
                    .as_ref()
                    .ok_or(TransportFailure::Protocol)?
                    .id
                    .clone();
                for attention in hydration.permissions.values_mut() {
                    attention.turn_id.clone_from(&current_turn_id);
                }
            }
        }
        self.emit(ProviderEvent::Ready {
            run_id: self.run_id(),
            session_id,
        })
        .await;
        if self.current_turn.is_some() {
            self.arm_current_turn(None)
                .await
                .map_err(|()| TransportFailure::Protocol)?;
        }
        if let Some(mut hydration) = hydration {
            for (_, attention) in std::mem::take(&mut hydration.permissions) {
                self.emit(ProviderEvent::AttentionRequested {
                    run_id: self.run_id(),
                    attention,
                })
                .await;
            }
            if let Some((_, question)) = hydration.questions.into_iter().next() {
                let attention = self
                    .question_attention(&question)
                    .map_err(|()| TransportFailure::Protocol)?;
                self.emit(ProviderEvent::AttentionRequested {
                    run_id: self.run_id(),
                    attention,
                })
                .await;
                return Err(match self.post_abort_ack().await {
                    Ok(()) | Err(AbortAckError::Definite) => TransportFailure::Protocol,
                    Err(AbortAckError::Ambiguous) => TransportFailure::DeliveryUnknown,
                });
            }
        }
        if !initial_input.trim().is_empty() {
            self.send_turn(initial_input).await?;
        }
        Ok(())
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        directory: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base_url))
            .basic_auth(&self.username, Some(&self.password));
        if let Some(directory) = directory {
            request = request.query(&[("directory", directory)]);
        }
        request
    }

    async fn get_json(&self, path: &str, directory: Option<&str>) -> Result<Value, ()> {
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            let response = self
                .request(Method::GET, path, directory)
                .send()
                .await
                .map_err(|_| ())?;
            response_json_bounded(response, MAX_HTTP_BODY_BYTES).await
        })
        .await
        .map_err(|_| ())?
    }

    async fn load_session_status(&self) -> Result<SessionPhase, ()> {
        let directory = self
            .start
            .as_ref()
            .and_then(|start| start.cwd.to_str())
            .ok_or(())?;
        let statuses = self.get_json("/session/status", Some(directory)).await?;
        let statuses = statuses.as_object().ok_or(())?;
        let Some(session_id) = self.session_id.as_deref() else {
            return Err(());
        };
        let status = match statuses.get(session_id) {
            Some(value) => value.get("type").and_then(Value::as_str).ok_or(())?,
            None => "idle",
        };
        match status {
            "idle" => Ok(SessionPhase::Idle),
            "busy" => Ok(SessionPhase::Busy),
            "retry" => Ok(SessionPhase::Retry),
            _ => Err(()),
        }
    }

    async fn load_message_baseline(&mut self) -> Result<Option<String>, ()> {
        let session_id = self.session_id.clone().ok_or(())?;
        let directory = self
            .start
            .as_ref()
            .and_then(|start| start.cwd.to_str())
            .ok_or(())?;
        let snapshot = self
            .get_json(&format!("/session/{session_id}/message"), Some(directory))
            .await?;
        let messages = snapshot.as_array().ok_or(())?;
        if messages.len() > MAX_TRACKED_MESSAGES {
            return Err(());
        }
        let mut roles = BTreeMap::new();
        let mut parts = BTreeMap::new();
        let mut part_messages = BTreeMap::new();
        let mut completed_parts = BTreeSet::new();
        let mut total_parts = 0_usize;
        let mut parts_bytes = 0_usize;
        let mut latest_assistant = None;
        for message in messages {
            let info = message.get("info").ok_or(())?;
            if info.get("sessionID").and_then(Value::as_str) != Some(session_id.as_str()) {
                return Err(());
            }
            let message_id = bounded_identifier(info.get("id"))?;
            let role = match info.get("role").and_then(Value::as_str) {
                Some("user") => MessageRole::User,
                Some("assistant") => {
                    latest_assistant = Some(message_id.to_owned());
                    MessageRole::Assistant
                }
                _ => return Err(()),
            };
            roles.insert(message_id.to_owned(), role);
            let message_parts = message.get("parts").and_then(Value::as_array).ok_or(())?;
            total_parts = total_parts.checked_add(message_parts.len()).ok_or(())?;
            if total_parts > MAX_TRACKED_PARTS {
                return Err(());
            }
            for part in message_parts {
                if part.get("sessionID").and_then(Value::as_str) != Some(session_id.as_str())
                    || part.get("messageID").and_then(Value::as_str) != Some(message_id)
                {
                    return Err(());
                }
                if part.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let part_id = bounded_identifier(part.get("id"))?;
                let text = part.get("text").and_then(Value::as_str).ok_or(())?;
                if text.len() > MAX_PROVIDER_FRAME_BYTES {
                    return Err(());
                }
                match part_messages.get(part_id) {
                    Some(owner) if owner != message_id => return Err(()),
                    _ => {}
                }
                let previous_bytes = parts.get(part_id).map_or(0, String::len);
                parts_bytes = parts_bytes
                    .checked_sub(previous_bytes)
                    .and_then(|total| total.checked_add(text.len()))
                    .filter(|total| *total <= MAX_SESSION_PART_BYTES)
                    .ok_or(())?;
                parts.insert(part_id.to_owned(), text.to_owned());
                part_messages.insert(part_id.to_owned(), message_id.to_owned());
                if part.pointer("/time/end").is_some_and(Value::is_number) {
                    completed_parts.insert(part_id.to_owned());
                }
            }
        }
        self.message_roles = roles;
        self.parts = parts;
        self.parts_bytes = parts_bytes;
        self.part_messages = part_messages;
        self.completed_parts = completed_parts;
        Ok(latest_assistant)
    }

    async fn hydrate_resume(
        &mut self,
        directory: &str,
    ) -> Result<ResumeHydration, TransportFailure> {
        let phase = self
            .load_session_status()
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        let permission_snapshot = self
            .get_json("/permission", Some(directory))
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        let question_snapshot = self
            .get_json("/question", Some(directory))
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        let latest_assistant = self
            .load_message_baseline()
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        let permissions = permission_snapshot
            .as_array()
            .ok_or(TransportFailure::Protocol)?;
        let questions = question_snapshot
            .as_array()
            .ok_or(TransportFailure::Protocol)?;
        if questions.len() > MAX_TRACKED_MESSAGES {
            return Err(TransportFailure::Protocol);
        }
        let mut hydration = ResumeHydration {
            phase,
            latest_assistant,
            permissions: BTreeMap::new(),
            questions: BTreeMap::new(),
        };
        for permission in permissions {
            if let Some(attention) = self
                .register_permission(permission)
                .map_err(|()| TransportFailure::Protocol)?
            {
                let RpcId::String(permission_id) = &attention.token.rpc_id else {
                    return Err(TransportFailure::Protocol);
                };
                hydration
                    .permissions
                    .insert(permission_id.clone(), attention);
            }
        }
        for question in questions {
            self.merge_initial_question(question, &mut hydration)
                .map_err(|()| TransportFailure::Protocol)?;
        }
        self.merge_initial_sse(&mut hydration)?;
        hydration.latest_assistant = self
            .load_message_baseline()
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        hydration.phase = self
            .load_session_status()
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        self.merge_initial_sse(&mut hydration)?;
        Ok(hydration)
    }

    fn merge_initial_sse(
        &mut self,
        hydration: &mut ResumeHydration,
    ) -> Result<(), TransportFailure> {
        loop {
            let message = match self
                .sse_rx
                .as_mut()
                .ok_or(TransportFailure::Protocol)?
                .try_recv()
            {
                Ok(message) => message,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(()),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(TransportFailure::Disconnected);
                }
            };
            let event = match message {
                SseMessage::Event(event) => event,
                SseMessage::Closed => return Err(TransportFailure::Disconnected),
                SseMessage::Invalid => return Err(TransportFailure::Protocol),
            };
            self.merge_initial_event(&event, hydration)
                .map_err(|()| TransportFailure::Protocol)?;
        }
    }

    fn merge_initial_event(
        &mut self,
        event: &Value,
        hydration: &mut ResumeHydration,
    ) -> Result<(), ()> {
        if !self.accept_event_id(event)? {
            return Ok(());
        }
        match event.get("type").and_then(Value::as_str).ok_or(())? {
            "server.connected" | "server.heartbeat" => Ok(()),
            "session.status" => {
                let properties = event.get("properties").ok_or(())?;
                if !self.matches_session(properties.get("sessionID")) {
                    return Ok(());
                }
                hydration.phase = match properties
                    .pointer("/status/type")
                    .and_then(Value::as_str)
                    .ok_or(())?
                {
                    "idle" => SessionPhase::Idle,
                    "busy" => SessionPhase::Busy,
                    "retry" => SessionPhase::Retry,
                    _ => return Err(()),
                };
                Ok(())
            }
            "permission.asked" => {
                let properties = event.get("properties").ok_or(())?;
                if let Some(attention) = self.register_permission(properties)? {
                    let RpcId::String(permission_id) = &attention.token.rpc_id else {
                        return Err(());
                    };
                    hydration
                        .permissions
                        .insert(permission_id.clone(), attention);
                }
                Ok(())
            }
            "permission.replied" => {
                let properties = event.get("properties").ok_or(())?;
                if !self.matches_session(properties.get("sessionID")) {
                    return Ok(());
                }
                let request_id = bounded_identifier(properties.get("requestID"))?;
                if !matches!(
                    properties.get("reply").and_then(Value::as_str),
                    Some("once" | "always" | "reject")
                ) {
                    return Err(());
                }
                self.pending_permissions.remove(request_id);
                hydration.permissions.remove(request_id);
                Ok(())
            }
            "question.asked" => {
                let properties = event.get("properties").ok_or(())?;
                self.merge_initial_question(properties, hydration)
            }
            "question.replied" | "question.rejected" => {
                let properties = event.get("properties").ok_or(())?;
                if !self.matches_session(properties.get("sessionID")) {
                    return Ok(());
                }
                let request_id = bounded_identifier(properties.get("requestID"))?;
                hydration.questions.remove(request_id);
                Ok(())
            }
            "message.updated" => {
                let info = event.pointer("/properties/info").ok_or(())?;
                if !self.matches_session(info.get("sessionID")) {
                    return Ok(());
                }
                let message_id = bounded_identifier(info.get("id"))?;
                let role = match info.get("role").and_then(Value::as_str) {
                    Some("user") => MessageRole::User,
                    Some("assistant") => {
                        hydration.latest_assistant = Some(message_id.to_owned());
                        MessageRole::Assistant
                    }
                    _ => return Err(()),
                };
                if !self.message_roles.contains_key(message_id)
                    && self.message_roles.len() >= MAX_TRACKED_MESSAGES
                {
                    return Err(());
                }
                self.message_roles.insert(message_id.to_owned(), role);
                Ok(())
            }
            "message.part.updated" => self.merge_initial_part(event),
            "message.part.delta" => self.merge_initial_delta(event),
            "message.part.removed" => self.handle_part_removed(event),
            "message.removed" => self.handle_message_removed(event),
            "session.error" => Err(()),
            _ => Ok(()),
        }
    }

    fn merge_initial_question(
        &self,
        properties: &Value,
        hydration: &mut ResumeHydration,
    ) -> Result<(), ()> {
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(());
        }
        self.question_attention(properties)?;
        let request_id = bounded_identifier(properties.get("id"))?;
        hydration
            .questions
            .insert(request_id.to_owned(), properties.clone());
        Ok(())
    }

    fn merge_initial_part(&mut self, event: &Value) -> Result<(), ()> {
        let properties = event.get("properties").ok_or(())?;
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(());
        }
        if !properties.get("time").is_some_and(Value::is_number) {
            return Err(());
        }
        let part = properties.get("part").ok_or(())?;
        if !self.matches_session(part.get("sessionID")) {
            return Err(());
        }
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return Ok(());
        }
        let message_id = bounded_identifier(part.get("messageID"))?;
        if !self.message_roles.contains_key(message_id) {
            return Err(());
        }
        let part_id = bounded_identifier(part.get("id"))?;
        let new_owner = match self.part_messages.get(part_id) {
            Some(owner) if owner != message_id => return Err(()),
            Some(_) => false,
            None => true,
        };
        let full = part.get("text").and_then(Value::as_str).ok_or(())?;
        if full.len() > MAX_PROVIDER_FRAME_BYTES {
            return Err(());
        }
        if !self.parts.contains_key(part_id) && self.parts.len() >= MAX_TRACKED_PARTS {
            return Err(());
        }
        match self.parts.get(part_id) {
            Some(previous) if full.starts_with(previous) => {
                self.replace_part_text(part_id, full.to_owned())?;
            }
            Some(previous) if previous.starts_with(full) => {}
            Some(_) => return Err(()),
            None => {
                self.replace_part_text(part_id, full.to_owned())?;
            }
        }
        if new_owner {
            self.part_messages
                .insert(part_id.to_owned(), message_id.to_owned());
        }
        if part.pointer("/time/end").is_some_and(Value::is_number) {
            self.completed_parts.insert(part_id.to_owned());
        }
        Ok(())
    }

    fn merge_initial_delta(&mut self, event: &Value) -> Result<(), ()> {
        let properties = event.get("properties").ok_or(())?;
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(());
        }
        let message_id = bounded_identifier(properties.get("messageID"))?;
        let part_id = bounded_identifier(properties.get("partID"))?;
        if properties.get("field").and_then(Value::as_str) != Some("text")
            || !matches!(
                self.message_roles.get(message_id),
                Some(MessageRole::Assistant)
            )
        {
            return Err(());
        }
        let delta = properties.get("delta").and_then(Value::as_str).ok_or(())?;
        if delta.len() > MAX_PROVIDER_FRAME_BYTES {
            return Err(());
        }
        if self.part_messages.get(part_id).map(String::as_str) != Some(message_id) {
            return Err(());
        }
        if self.completed_parts.contains(part_id) {
            return Ok(());
        }
        if self
            .parts
            .get(part_id)
            .ok_or(())?
            .len()
            .checked_add(delta.len())
            .filter(|length| *length <= MAX_TURN_OUTPUT_BYTES)
            .is_none()
        {
            return Err(());
        }
        self.append_part_text(part_id, delta)?;
        Ok(())
    }

    fn replace_part_text(&mut self, part_id: &str, text: String) -> Result<(), ()> {
        let previous_bytes = self.parts.get(part_id).map_or(0, String::len);
        let updated_bytes = self
            .parts_bytes
            .checked_sub(previous_bytes)
            .and_then(|total| total.checked_add(text.len()))
            .filter(|total| *total <= MAX_SESSION_PART_BYTES)
            .ok_or(())?;
        self.parts.insert(part_id.to_owned(), text);
        self.parts_bytes = updated_bytes;
        Ok(())
    }

    fn append_part_text(&mut self, part_id: &str, delta: &str) -> Result<(), ()> {
        let updated_bytes = self
            .parts_bytes
            .checked_add(delta.len())
            .filter(|total| *total <= MAX_SESSION_PART_BYTES)
            .ok_or(())?;
        self.parts.get_mut(part_id).ok_or(())?.push_str(delta);
        self.parts_bytes = updated_bytes;
        Ok(())
    }

    fn remove_part_text(&mut self, part_id: &str) -> Result<(), ()> {
        if let Some(text) = self.parts.remove(part_id) {
            self.parts_bytes = self.parts_bytes.checked_sub(text.len()).ok_or(())?;
        }
        self.part_messages.remove(part_id);
        self.completed_parts.remove(part_id);
        Ok(())
    }

    fn clear_message_tracking(&mut self) {
        self.parts.clear();
        self.parts_bytes = 0;
        self.part_messages.clear();
        self.completed_parts.clear();
        self.message_roles.clear();
    }

    fn handle_part_removed(&mut self, event: &Value) -> Result<(), ()> {
        let properties = event.get("properties").ok_or(())?;
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(());
        }
        let message_id = bounded_identifier(properties.get("messageID"))?;
        let part_id = bounded_identifier(properties.get("partID"))?;
        if self
            .part_messages
            .get(part_id)
            .is_some_and(|owner| owner != message_id)
        {
            return Err(());
        }
        self.remove_part_text(part_id)
    }

    fn handle_message_removed(&mut self, event: &Value) -> Result<(), ()> {
        let properties = event.get("properties").ok_or(())?;
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(());
        }
        let message_id = bounded_identifier(properties.get("messageID"))?;
        let part_ids = self
            .part_messages
            .iter()
            .filter_map(|(part_id, owner)| (owner == message_id).then_some(part_id.clone()))
            .collect::<Vec<_>>();
        for part_id in part_ids {
            self.remove_part_text(&part_id)?;
        }
        self.message_roles.remove(message_id);
        if let Some(turn) = self.current_turn.as_mut() {
            turn.assistant_messages.remove(message_id);
        }
        Ok(())
    }

    async fn open_sse(&mut self, directory: &str) -> Result<(), ()> {
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.request(Method::GET, "/event", Some(directory)).send(),
        )
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
        if response.status() != StatusCode::OK
            || !response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(());
        }
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(read_sse(response, sender));
        self.sse_rx = Some(receiver);
        Ok(())
    }

    async fn send_turn(&mut self, input: String) -> Result<(), TransportFailure> {
        if self.current_turn.is_some() || input.len() > MAX_PROVIDER_FRAME_BYTES {
            return Err(TransportFailure::CommandRejected);
        }
        let session_id = self
            .session_id
            .clone()
            .ok_or(TransportFailure::CommandRejected)?;
        let status = self
            .load_session_status()
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        if status != SessionPhase::Idle {
            return Err(TransportFailure::CommandRejected);
        }
        self.load_message_baseline()
            .await
            .map_err(|()| TransportFailure::Protocol)?;
        let turn_id = format!("turn-{}", self.next_turn_id);
        self.next_turn_id += 1;
        self.current_turn = Some(CurrentTurn {
            id: turn_id,
            armed: false,
            working_emitted: false,
            interrupt_requested: false,
            assistant_messages: BTreeSet::new(),
            output_bytes: 0,
            deadline: tokio::time::Instant::now() + TURN_IDLE_TIMEOUT,
        });
        let directory = self
            .start
            .as_ref()
            .and_then(|start| start.cwd.to_str())
            .ok_or(TransportFailure::CommandRejected)?;
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.request(
                Method::POST,
                &format!("/session/{session_id}/prompt_async"),
                Some(directory),
            )
            .json(&json!({"parts": [{"type": "text", "text": input}]}))
            .send(),
        )
        .await
        .map_err(|_| TransportFailure::DeliveryUnknown)?
        .map_err(|_| TransportFailure::DeliveryUnknown)?;
        if response.status() != StatusCode::NO_CONTENT {
            self.current_turn = None;
            return Err(TransportFailure::CommandRejected);
        }
        Ok(())
    }

    async fn handle_command(&mut self, command: ProviderCommand) -> bool {
        match command {
            ProviderCommand::SendTurn { input } if !self.quiesced => {
                if let Err(reason) = self.send_turn(input).await {
                    self.fail(reason).await;
                    if reason == TransportFailure::DeliveryUnknown {
                        return true;
                    }
                }
            }
            ProviderCommand::Respond { token, response } => {
                match self.respond_permission(token, response).await {
                    Ok(()) => {}
                    Err(ResponseSendError::Definite) => {
                        self.fail(TransportFailure::CommandRejected).await;
                    }
                    Err(ResponseSendError::Ambiguous) => {
                        self.fail(TransportFailure::DeliveryUnknown).await;
                        return true;
                    }
                }
            }
            ProviderCommand::Interrupt => match self.abort_current_turn().await {
                Ok(()) => {}
                Err(AbortAckError::Definite) => {
                    self.fail(TransportFailure::Protocol).await;
                    return true;
                }
                Err(AbortAckError::Ambiguous) => {
                    self.fail(TransportFailure::DeliveryUnknown).await;
                    return true;
                }
            },
            ProviderCommand::Quiesce => self.quiesced = true,
            ProviderCommand::Shutdown => {
                self.emit(ProviderEvent::Stopped {
                    run_id: self.run_id(),
                })
                .await;
                return true;
            }
            _ => self.fail(TransportFailure::CommandRejected).await,
        }
        false
    }

    async fn handle_event(&mut self, event: &Value) -> Result<(), EventFailure> {
        if !self
            .accept_event_id(event)
            .map_err(|()| EventFailure::Protocol)?
        {
            return Ok(());
        }
        if event.get("type").and_then(Value::as_str) == Some("question.asked") {
            return if self.handle_unsupported_question(event).await? {
                Err(EventFailure::Protocol)
            } else {
                Ok(())
            };
        }
        self.handle_protocol_event(event)
            .await
            .map_err(|()| EventFailure::Protocol)
    }

    fn accept_event_id(&mut self, event: &Value) -> Result<bool, ()> {
        let Some(event_id) = event.get("id") else {
            return Ok(true);
        };
        let event_id = bounded_identifier(Some(event_id))?;
        if self.seen_event_ids.contains(event_id) {
            return Ok(false);
        }
        if self.seen_event_order.len() >= MAX_TRACKED_EVENTS {
            let oldest = self.seen_event_order.pop_front().ok_or(())?;
            self.seen_event_ids.remove(&oldest);
        }
        self.seen_event_ids.insert(event_id.to_owned());
        self.seen_event_order.push_back(event_id.to_owned());
        Ok(true)
    }

    async fn handle_protocol_event(&mut self, event: &Value) -> Result<(), ()> {
        let event_type = event.get("type").and_then(Value::as_str).ok_or(())?;
        match event_type {
            "server.connected" | "server.heartbeat" => Ok(()),
            "session.status" => {
                let properties = event.get("properties").ok_or(())?;
                if !self.matches_session(properties.get("sessionID")) {
                    return Ok(());
                }
                let status = properties
                    .pointer("/status/type")
                    .and_then(Value::as_str)
                    .ok_or(())?;
                match status {
                    "busy" | "retry" => {
                        if let Some(turn) = self.current_turn.as_mut() {
                            if turn.armed {
                                turn.deadline = tokio::time::Instant::now() + TURN_IDLE_TIMEOUT;
                            }
                        }
                        Ok(())
                    }
                    "idle" => {
                        let Some(turn) = self.current_turn.as_ref() else {
                            return Ok(());
                        };
                        if !turn.armed {
                            return Ok(());
                        }
                        let turn = self.current_turn.take().ok_or(())?;
                        self.clear_message_tracking();
                        self.emit(ProviderEvent::TurnCompleted {
                            run_id: self.run_id(),
                            turn_id: turn.id,
                            outcome: if turn.interrupt_requested {
                                TurnOutcome::Interrupted
                            } else {
                                TurnOutcome::Completed
                            },
                        })
                        .await;
                        Ok(())
                    }
                    _ => Err(()),
                }
            }
            "message.part.updated" => self.handle_part_updated(event).await,
            "message.part.delta" => self.handle_part_delta(event).await,
            "message.part.removed" => self.handle_part_removed(event),
            "message.removed" => self.handle_message_removed(event),
            "message.updated" => self.handle_message_updated(event).await,
            "permission.asked" => self.handle_permission_asked(event).await,
            "permission.replied" => self.handle_permission_replied(event).await,
            "session.error" => self.handle_session_error(event).await,
            _ => Ok(()),
        }
    }

    async fn handle_message_updated(&mut self, event: &Value) -> Result<(), ()> {
        let info = event.pointer("/properties/info").ok_or(())?;
        if !self.matches_session(info.get("sessionID")) {
            return Ok(());
        }
        let message_id = bounded_identifier(info.get("id"))?;
        let role = match info.get("role").and_then(Value::as_str) {
            Some("user") => MessageRole::User,
            Some("assistant") => MessageRole::Assistant,
            _ => return Err(()),
        };
        let is_new = !self.message_roles.contains_key(message_id);
        if is_new && self.message_roles.len() >= MAX_TRACKED_MESSAGES {
            return Err(());
        }
        self.message_roles.insert(message_id.to_owned(), role);
        if is_new && self.current_turn.is_none() && self.follow_external_turns {
            let turn_id = format!("turn-{}", self.next_turn_id);
            self.next_turn_id += 1;
            self.current_turn = Some(CurrentTurn {
                id: turn_id,
                armed: false,
                working_emitted: false,
                interrupt_requested: false,
                assistant_messages: BTreeSet::new(),
                output_bytes: 0,
                deadline: tokio::time::Instant::now() + TURN_IDLE_TIMEOUT,
            });
        }
        if is_new && self.current_turn.is_some() {
            self.arm_current_turn(if matches!(role, MessageRole::Assistant) {
                Some(message_id.to_owned())
            } else {
                None
            })
            .await?;
        }
        Ok(())
    }

    async fn handle_part_updated(&mut self, event: &Value) -> Result<(), ()> {
        let properties = event.get("properties").ok_or(())?;
        if !self.matches_session(properties.get("sessionID"))
            || !properties.get("time").is_some_and(Value::is_number)
        {
            return if self.matches_session(properties.get("sessionID")) {
                Err(())
            } else {
                Ok(())
            };
        }
        let part = properties.get("part").ok_or(())?;
        if !self.matches_session(part.get("sessionID")) {
            return Err(());
        }
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return Ok(());
        }
        let message_id = bounded_identifier(part.get("messageID"))?;
        let role = *self.message_roles.get(message_id).ok_or(())?;
        let part_id = bounded_identifier(part.get("id"))?;
        let new_owner = match self.part_messages.get(part_id) {
            Some(owner) if owner != message_id => return Err(()),
            Some(_) => false,
            None => true,
        };
        let full = part.get("text").and_then(Value::as_str).ok_or(())?;
        if full.len() > MAX_PROVIDER_FRAME_BYTES
            || (!self.parts.contains_key(part_id) && self.parts.len() >= MAX_TRACKED_PARTS)
        {
            return Err(());
        }
        let previous = self
            .parts
            .get(part_id)
            .map(String::as_str)
            .unwrap_or_default();
        if self.completed_parts.contains(part_id) {
            return if previous.starts_with(full) {
                Ok(())
            } else {
                Err(())
            };
        }
        if !full.starts_with(previous) {
            return Err(());
        }
        let suffix = full[previous.len()..].to_owned();
        self.replace_part_text(part_id, full.to_owned())?;
        if new_owner {
            self.part_messages
                .insert(part_id.to_owned(), message_id.to_owned());
        }
        if part.pointer("/time/end").is_some_and(Value::is_number) {
            self.completed_parts.insert(part_id.to_owned());
        }
        if matches!(role, MessageRole::Assistant)
            && self
                .current_turn
                .as_ref()
                .is_some_and(|turn| turn.assistant_messages.contains(message_id))
        {
            self.emit_output_delta(suffix).await?;
        }
        Ok(())
    }

    async fn handle_part_delta(&mut self, event: &Value) -> Result<(), ()> {
        let properties = event.get("properties").ok_or(())?;
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(());
        }
        let message_id = bounded_identifier(properties.get("messageID"))?;
        let part_id = bounded_identifier(properties.get("partID"))?;
        if properties.get("field").and_then(Value::as_str) != Some("text") {
            return Err(());
        }
        let delta = properties.get("delta").and_then(Value::as_str).ok_or(())?;
        if delta.len() > MAX_PROVIDER_FRAME_BYTES {
            return Err(());
        }
        match self.message_roles.get(message_id) {
            Some(MessageRole::User) => return Ok(()),
            Some(MessageRole::Assistant) => {}
            None => return Err(()),
        }
        let Some(turn) = self.current_turn.as_ref() else {
            return Ok(());
        };
        if !turn.assistant_messages.contains(message_id) {
            return Ok(());
        }
        if self.part_messages.get(part_id).map(String::as_str) != Some(message_id) {
            return Err(());
        }
        if self.completed_parts.contains(part_id) {
            return Ok(());
        }
        if self
            .parts
            .get(part_id)
            .ok_or(())?
            .len()
            .checked_add(delta.len())
            .filter(|length| *length <= MAX_TURN_OUTPUT_BYTES)
            .is_none()
        {
            return Err(());
        }
        self.append_part_text(part_id, delta)?;
        self.emit_output_delta(delta.to_owned()).await
    }

    async fn emit_output_delta(&mut self, text: String) -> Result<(), ()> {
        if text.is_empty() {
            return Ok(());
        }
        let turn = self.current_turn.as_mut().ok_or(())?;
        turn.output_bytes = turn.output_bytes.checked_add(text.len()).ok_or(())?;
        if turn.output_bytes > MAX_TURN_OUTPUT_BYTES {
            return Err(());
        }
        let turn_id = turn.id.clone();
        turn.deadline = tokio::time::Instant::now() + TURN_IDLE_TIMEOUT;
        let run_id = self.run_id();
        let mut remaining = text.as_str();
        while !remaining.is_empty() {
            let mut end = remaining.len().min(MAX_VISIBLE_TEXT_BYTES);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            let (chunk, rest) = remaining.split_at(end);
            self.emit(ProviderEvent::OutputDelta {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                text: chunk.to_owned(),
            })
            .await;
            remaining = rest;
        }
        Ok(())
    }

    fn refresh_turn_deadline(&mut self) {
        if let Some(turn) = self.current_turn.as_mut() {
            turn.deadline = tokio::time::Instant::now() + TURN_IDLE_TIMEOUT;
        }
    }

    async fn arm_current_turn(&mut self, assistant_message: Option<String>) -> Result<(), ()> {
        let turn = self.current_turn.as_mut().ok_or(())?;
        turn.armed = true;
        turn.deadline = tokio::time::Instant::now() + TURN_IDLE_TIMEOUT;
        if let Some(message_id) = assistant_message {
            if turn.assistant_messages.len() >= MAX_TRACKED_MESSAGES {
                return Err(());
            }
            turn.assistant_messages.insert(message_id);
        }
        if turn.working_emitted {
            return Ok(());
        }
        turn.working_emitted = true;
        let turn_id = turn.id.clone();
        self.emit(ProviderEvent::Working {
            run_id: self.run_id(),
            turn_id,
        })
        .await;
        Ok(())
    }

    async fn abort_current_turn(&mut self) -> Result<(), AbortAckError> {
        let Some(turn) = self.current_turn.as_mut() else {
            return Err(AbortAckError::Definite);
        };
        if turn.interrupt_requested {
            return Err(AbortAckError::Definite);
        }
        turn.interrupt_requested = true;
        self.post_abort_ack().await?;
        self.current_turn
            .as_mut()
            .ok_or(AbortAckError::Definite)?
            .armed = true;
        Ok(())
    }

    async fn post_abort_ack(&self) -> Result<(), AbortAckError> {
        let session_id = self.session_id.clone().ok_or(AbortAckError::Definite)?;
        let directory = self
            .start
            .as_ref()
            .and_then(|start| start.cwd.to_str())
            .ok_or(AbortAckError::Definite)?;
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.request(
                Method::POST,
                &format!("/session/{session_id}/abort"),
                Some(directory),
            )
            .send(),
        )
        .await
        .map_err(|_| AbortAckError::Ambiguous)?
        .map_err(|_| AbortAckError::Ambiguous)?;
        if !response.status().is_success() {
            return Err(AbortAckError::Definite);
        }
        let value = tokio::time::timeout(
            REQUEST_TIMEOUT,
            response_json_bounded(response, MAX_HTTP_BODY_BYTES),
        )
        .await
        .map_err(|_| AbortAckError::Ambiguous)?
        .map_err(|()| AbortAckError::Ambiguous)?;
        if value != Value::Bool(true) {
            return if value == Value::Bool(false) {
                Err(AbortAckError::Definite)
            } else {
                Err(AbortAckError::Ambiguous)
            };
        }
        Ok(())
    }

    async fn handle_session_error(&mut self, event: &Value) -> Result<(), ()> {
        let properties = event.get("properties").ok_or(())?;
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(());
        }
        let Some(turn) = self.current_turn.take() else {
            return Ok(());
        };
        self.clear_message_tracking();
        self.emit(ProviderEvent::TurnCompleted {
            run_id: self.run_id(),
            turn_id: turn.id,
            outcome: TurnOutcome::Failed,
        })
        .await;
        Ok(())
    }

    async fn handle_unsupported_question(&mut self, event: &Value) -> Result<bool, EventFailure> {
        let properties = event.get("properties").ok_or(EventFailure::Protocol)?;
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(false);
        }
        let attention = self
            .question_attention(properties)
            .map_err(|()| EventFailure::Protocol)?;
        self.emit(ProviderEvent::AttentionRequested {
            run_id: self.run_id(),
            attention,
        })
        .await;

        self.post_abort_ack().await.map_err(EventFailure::from)?;
        Ok(true)
    }

    fn question_attention(&self, properties: &Value) -> Result<ProviderAttention, ()> {
        let request_id = bounded_identifier(properties.get("id"))?;
        let questions = properties
            .get("questions")
            .and_then(Value::as_array)
            .ok_or(())?;
        if questions.is_empty() || questions.len() > MAX_PERMISSION_PATTERNS {
            return Err(());
        }
        if let Some(tool) = properties.get("tool") {
            let tool = tool.as_object().ok_or(())?;
            bounded_identifier(tool.get("messageID"))?;
            bounded_identifier(tool.get("callID"))?;
        }
        let rpc_id = RpcId::String(request_id.to_owned());
        Ok(ProviderAttention {
            token: ResponseToken {
                request_id: rpc_id.audit_id(),
                rpc_id,
                method: "opencode/question/unsupported".to_owned(),
            },
            class: AttentionClass::UserInput,
            thread_id: self.session_id.clone().ok_or(())?,
            turn_id: self
                .current_turn
                .as_ref()
                .map(|turn| turn.id.clone())
                .unwrap_or_else(|| "pending".to_owned()),
            item_id: request_id.to_owned(),
            requested_action:
                "Structured question is unsupported by the tested OpenCode server API".to_owned(),
            questions: Vec::new(),
        })
    }

    async fn handle_permission_asked(&mut self, event: &Value) -> Result<(), ()> {
        let permission = event.get("properties").ok_or(())?;
        let Some(attention) = self.register_permission(permission)? else {
            return Ok(());
        };
        self.emit(ProviderEvent::AttentionRequested {
            run_id: self.run_id(),
            attention,
        })
        .await;
        Ok(())
    }

    fn register_permission(&mut self, permission: &Value) -> Result<Option<ProviderAttention>, ()> {
        if !self.matches_session(permission.get("sessionID")) {
            return Ok(None);
        }
        let permission_id = bounded_identifier(permission.get("id"))?;
        if self.pending_permissions.contains_key(permission_id) {
            return Ok(None);
        }
        if self.pending_permissions.len() >= MAX_PENDING_PERMISSIONS {
            return Err(());
        }
        let permission_name = bounded_text(permission.get("permission"), MAX_ATTENTION_TEXT_BYTES)?;
        let patterns = permission
            .get("patterns")
            .and_then(Value::as_array)
            .ok_or(())?;
        if patterns.len() > MAX_PERMISSION_PATTERNS
            || !permission.get("metadata").is_some_and(Value::is_object)
            || !permission.get("always").is_some_and(Value::is_array)
        {
            return Err(());
        }
        let mut requested_action = permission_name.to_owned();
        for (index, pattern) in patterns.iter().enumerate() {
            let pattern = bounded_text(Some(pattern), MAX_ATTENTION_TEXT_BYTES)?;
            requested_action.push_str(if index == 0 { ": " } else { ", " });
            requested_action.push_str(pattern);
            if requested_action.len() > MAX_ATTENTION_TEXT_BYTES {
                return Err(());
            }
        }
        if requested_action.len() > MAX_ATTENTION_TEXT_BYTES {
            return Err(());
        }
        let rpc_id = RpcId::String(permission_id.to_owned());
        let audit_id = rpc_id.audit_id();
        let item_id = match permission.get("tool") {
            Some(tool) => {
                let tool = tool.as_object().ok_or(())?;
                bounded_identifier(tool.get("messageID"))?;
                bounded_identifier(tool.get("callID"))?
            }
            None => permission_id,
        };
        let token = ResponseToken {
            rpc_id,
            method: PERMISSION_METHOD.to_owned(),
            request_id: audit_id.clone(),
        };
        self.pending_permissions.insert(
            permission_id.to_owned(),
            PendingPermission {
                audit_id,
                responding: false,
            },
        );
        self.refresh_turn_deadline();
        Ok(Some(ProviderAttention {
            token,
            class: AttentionClass::PermissionApproval,
            thread_id: self.session_id.clone().ok_or(())?,
            turn_id: self
                .current_turn
                .as_ref()
                .map(|turn| turn.id.clone())
                .unwrap_or_else(|| "pending".to_owned()),
            item_id: item_id.to_owned(),
            requested_action,
            questions: Vec::new(),
        }))
    }

    async fn handle_permission_replied(&mut self, event: &Value) -> Result<(), ()> {
        let properties = event.get("properties").ok_or(())?;
        if !self.matches_session(properties.get("sessionID")) {
            return Ok(());
        }
        let permission_id = bounded_identifier(properties.get("requestID"))?;
        if !matches!(
            properties.get("reply").and_then(Value::as_str),
            Some("once" | "always" | "reject")
        ) {
            return Err(());
        }
        let Some(pending) = self.pending_permissions.remove(permission_id) else {
            return Ok(());
        };
        self.refresh_turn_deadline();
        self.emit(ProviderEvent::ResponseResolved {
            run_id: self.run_id(),
            request_id: pending.audit_id,
        })
        .await;
        Ok(())
    }

    async fn respond_permission(
        &mut self,
        token: ResponseToken,
        response: ProviderResponse,
    ) -> Result<(), ResponseSendError> {
        if token.method != PERMISSION_METHOD {
            return Err(ResponseSendError::Definite);
        }
        let RpcId::String(permission_id) = token.rpc_id else {
            return Err(ResponseSendError::Definite);
        };
        validate_identifier(&permission_id).map_err(|()| ResponseSendError::Definite)?;
        let pending = self
            .pending_permissions
            .get_mut(&permission_id)
            .ok_or(ResponseSendError::Definite)?;
        if pending.responding || pending.audit_id != token.request_id {
            return Err(ResponseSendError::Definite);
        }
        let wire_response =
            permission_response_value(&response).ok_or(ResponseSendError::Definite)?;
        pending.responding = true;
        let directory = self
            .start
            .as_ref()
            .and_then(|start| start.cwd.to_str())
            .ok_or(ResponseSendError::Definite)?;
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.request(
                Method::POST,
                &format!("/permission/{permission_id}/reply"),
                Some(directory),
            )
            .json(&json!({"reply": wire_response}))
            .send(),
        )
        .await
        .map_err(|_| ResponseSendError::Ambiguous)?
        .map_err(|_| ResponseSendError::Ambiguous)?;
        if !response.status().is_success() {
            if let Some(pending) = self.pending_permissions.get_mut(&permission_id) {
                pending.responding = false;
            }
            return Err(ResponseSendError::Definite);
        }
        let value = tokio::time::timeout(
            REQUEST_TIMEOUT,
            response_json_bounded(response, MAX_HTTP_BODY_BYTES),
        )
        .await
        .map_err(|_| ResponseSendError::Ambiguous)?
        .map_err(|()| ResponseSendError::Ambiguous)?;
        if value != Value::Bool(true) {
            return Err(ResponseSendError::Ambiguous);
        }
        Ok(())
    }

    fn matches_session(&self, value: Option<&Value>) -> bool {
        value.and_then(Value::as_str) == self.session_id.as_deref()
    }

    fn run_id(&self) -> String {
        self.run_id.clone().unwrap_or_default()
    }

    async fn emit(&self, event: ProviderEvent) {
        let _ = self.events.send(event).await;
    }

    async fn fail(&self, reason: TransportFailure) {
        self.emit(ProviderEvent::TransportFailed {
            run_id: self.run_id(),
            reason,
        })
        .await;
    }
}

enum ResponseSendError {
    Definite,
    Ambiguous,
}

enum AbortAckError {
    Definite,
    Ambiguous,
}

enum EventFailure {
    Protocol,
    DeliveryUnknown,
}

impl EventFailure {
    fn transport_failure(self) -> TransportFailure {
        match self {
            Self::Protocol => TransportFailure::Protocol,
            Self::DeliveryUnknown => TransportFailure::DeliveryUnknown,
        }
    }
}

impl From<AbortAckError> for EventFailure {
    fn from(value: AbortAckError) -> Self {
        match value {
            AbortAckError::Definite => Self::Protocol,
            AbortAckError::Ambiguous => Self::DeliveryUnknown,
        }
    }
}

async fn response_json_bounded(mut response: Response, limit: usize) -> Result<Value, ()> {
    if !response.status().is_success() {
        return Err(());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| ())
}

async fn read_sse(mut response: Response, sender: mpsc::Sender<SseMessage>) {
    let mut decoder = SseDecoder::default();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                for byte in chunk.iter().copied() {
                    match decoder.push_byte(byte) {
                        Ok(Some(value)) => {
                            if sender.send(SseMessage::Event(value)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(()) => {
                            let _ = sender.send(SseMessage::Invalid).await;
                            return;
                        }
                    }
                }
            }
            Ok(None) => {
                let _ = sender
                    .send(if decoder.finish().is_ok() {
                        SseMessage::Closed
                    } else {
                        SseMessage::Invalid
                    })
                    .await;
                return;
            }
            Err(_) => {
                let _ = sender.send(SseMessage::Closed).await;
                return;
            }
        }
    }
}

fn pending_separator_prefix_len(bytes: &[u8]) -> usize {
    let mut longest = 0;
    for separator in [b"\n\n".as_slice(), b"\r\n\r\n".as_slice()] {
        for length in 1..separator.len() {
            if bytes.ends_with(&separator[..length]) {
                longest = longest.max(length);
            }
        }
    }
    longest
}

fn parse_sse_frame(bytes: &[u8]) -> Result<Option<Value>, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    let mut data = String::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&data).map(Some).map_err(|_| ())
}

fn validate_identifier(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(());
    }
    Ok(())
}

fn bounded_identifier(value: Option<&Value>) -> Result<&str, ()> {
    let value = value.and_then(Value::as_str).ok_or(())?;
    validate_identifier(value)?;
    Ok(value)
}

fn bounded_text(value: Option<&Value>, max_bytes: usize) -> Result<&str, ()> {
    let value = value.and_then(Value::as_str).ok_or(())?;
    if value.is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(());
    }
    Ok(value)
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

async fn read_listening_url(stdout: &mut ChildStdout) -> Result<String, ()> {
    let mut pending = Vec::new();
    let mut total = 0_usize;
    loop {
        let mut chunk = [0_u8; 1024];
        let count = stdout.read(&mut chunk).await.map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        total = total.checked_add(count).ok_or(())?;
        if total > MAX_STARTUP_OUTPUT_BYTES {
            return Err(());
        }
        pending.extend_from_slice(&chunk[..count]);
        while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=position).collect::<Vec<_>>();
            if let Some(url) = parse_listening_url(&line) {
                return Ok(url);
            }
        }
    }
}

async fn drain_output(mut output: impl tokio::io::AsyncRead + Unpin) {
    let mut buffer = [0_u8; STDERR_DRAIN_BUFFER_BYTES];
    loop {
        match output.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, AtomicU8, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
        sync::{broadcast, mpsc},
    };

    use super::super::{
        AttentionClass, ProviderCommand, ProviderEvent, ProviderResponse, SandboxAccess,
        StartOrResume, TransportFailure, TurnOutcome,
    };

    const USERNAME: &str = "opencode";
    const PASSWORD: &str = "test-only-password";

    #[derive(Clone, Debug)]
    struct RequestAudit {
        method: String,
        target: String,
        authorization: Option<String>,
        body: Vec<u8>,
    }

    struct FakeServer {
        base_url: String,
        events: broadcast::Sender<String>,
        requests: Arc<Mutex<Vec<RequestAudit>>>,
        sse_connected: Arc<AtomicBool>,
        session_status: Arc<Mutex<Vec<serde_json::Value>>>,
        session_messages: Arc<Mutex<serde_json::Value>>,
        pending_permissions: Arc<Mutex<serde_json::Value>>,
        pending_questions: Arc<Mutex<serde_json::Value>>,
        message_snapshot_events: Arc<Mutex<Vec<serde_json::Value>>>,
        abort_response_mode: Arc<AtomicU8>,
    }

    impl FakeServer {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (events, _) = broadcast::channel(32);
            let requests = Arc::new(Mutex::new(Vec::new()));
            let sse_connected = Arc::new(AtomicBool::new(false));
            let session_status = Arc::new(Mutex::new(vec![json!({
                "ses_test": {"type": "idle"}
            })]));
            let session_messages = Arc::new(Mutex::new(json!([])));
            let pending_permissions = Arc::new(Mutex::new(json!([])));
            let pending_questions = Arc::new(Mutex::new(json!([])));
            let message_snapshot_events = Arc::new(Mutex::new(Vec::new()));
            let abort_response_mode = Arc::new(AtomicU8::new(0));
            let task_events = events.clone();
            let task_requests = Arc::clone(&requests);
            let task_sse_connected = Arc::clone(&sse_connected);
            let task_status = Arc::clone(&session_status);
            let task_messages = Arc::clone(&session_messages);
            let task_permissions = Arc::clone(&pending_permissions);
            let task_questions = Arc::clone(&pending_questions);
            let task_message_events = Arc::clone(&message_snapshot_events);
            let task_abort_mode = Arc::clone(&abort_response_mode);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let connection_events = task_events.clone();
                    let connection_requests = Arc::clone(&task_requests);
                    let connection_sse = Arc::clone(&task_sse_connected);
                    let connection_status = Arc::clone(&task_status);
                    let connection_messages = Arc::clone(&task_messages);
                    let connection_permissions = Arc::clone(&task_permissions);
                    let connection_questions = Arc::clone(&task_questions);
                    let connection_message_events = Arc::clone(&task_message_events);
                    let connection_abort_mode = Arc::clone(&task_abort_mode);
                    tokio::spawn(async move {
                        handle_connection(
                            stream,
                            connection_events,
                            connection_requests,
                            connection_sse,
                            connection_status,
                            connection_messages,
                            connection_permissions,
                            connection_questions,
                            connection_message_events,
                            connection_abort_mode,
                        )
                        .await;
                    });
                }
            });
            Self {
                base_url: format!("http://{address}"),
                events,
                requests,
                sse_connected,
                session_status,
                session_messages,
                pending_permissions,
                pending_questions,
                message_snapshot_events,
                abort_response_mode,
            }
        }

        fn audits(&self) -> Vec<RequestAudit> {
            self.requests.lock().unwrap().clone()
        }

        fn emit(&self, event: serde_json::Value) {
            let _ = self.events.send(event.to_string());
        }

        fn emit_raw(&self, event: String) {
            let _ = self.events.send(event);
        }

        fn close_sse(&self) {
            let _ = self.events.send("__close_sse__".to_owned());
        }

        fn set_messages(&self, value: serde_json::Value) {
            *self.session_messages.lock().unwrap() = value;
        }

        fn set_status(&self, value: serde_json::Value) {
            *self.session_status.lock().unwrap() = vec![value];
        }

        fn set_status_sequence(&self, values: Vec<serde_json::Value>) {
            assert!(!values.is_empty());
            *self.session_status.lock().unwrap() = values;
        }

        fn set_pending_permissions(&self, value: serde_json::Value) {
            *self.pending_permissions.lock().unwrap() = value;
        }

        fn set_pending_questions(&self, value: serde_json::Value) {
            *self.pending_questions.lock().unwrap() = value;
        }

        fn emit_during_next_message_snapshot(&self, values: Vec<serde_json::Value>) {
            *self.message_snapshot_events.lock().unwrap() = values;
        }

        fn drop_abort_response(&self) {
            self.abort_response_mode.store(1, Ordering::SeqCst);
        }

        fn reject_abort_with_http_error(&self) {
            self.abort_response_mode.store(2, Ordering::SeqCst);
        }

        fn reject_abort_with_false(&self) {
            self.abort_response_mode.store(3, Ordering::SeqCst);
        }
    }

    async fn handle_connection(
        mut stream: TcpStream,
        events: broadcast::Sender<String>,
        requests: Arc<Mutex<Vec<RequestAudit>>>,
        sse_connected: Arc<AtomicBool>,
        session_status: Arc<Mutex<Vec<serde_json::Value>>>,
        session_messages: Arc<Mutex<serde_json::Value>>,
        pending_permissions: Arc<Mutex<serde_json::Value>>,
        pending_questions: Arc<Mutex<serde_json::Value>>,
        message_snapshot_events: Arc<Mutex<Vec<serde_json::Value>>>,
        abort_response_mode: Arc<AtomicU8>,
    ) {
        let Some(request) = read_request(&mut stream).await else {
            return;
        };
        let authorized = request.authorization.as_deref()
            == Some(
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("{USERNAME}:{PASSWORD}"))
                )
                .as_str(),
            );
        requests.lock().unwrap().push(request.clone());
        if !authorized {
            write_response(&mut stream, 401, "text/plain", b"").await;
            return;
        }
        let path = request.target.split('?').next().unwrap_or_default();
        match (request.method.as_str(), path) {
            ("GET", "/global/health") => {
                write_json(
                    &mut stream,
                    200,
                    &json!({"healthy": true, "version": "1.18.3"}),
                )
                .await;
            }
            ("GET", "/doc") => {
                write_json(&mut stream, 200, &required_doc()).await;
            }
            ("POST", "/session") => {
                write_json(&mut stream, 200, &json!({"id": "ses_test"})).await;
            }
            ("GET", "/session/ses_test") => {
                write_json(
                    &mut stream,
                    200,
                    &json!({"id": "ses_test", "directory": "/tmp/project"}),
                )
                .await;
            }
            ("GET", "/session/status") => {
                let value = {
                    let mut values = session_status.lock().unwrap();
                    if values.len() > 1 {
                        values.remove(0)
                    } else {
                        values[0].clone()
                    }
                };
                write_json(&mut stream, 200, &value).await;
            }
            ("GET", "/session/ses_test/message") => {
                let snapshot_events = std::mem::take(&mut *message_snapshot_events.lock().unwrap());
                for event in &snapshot_events {
                    let _ = events.send(event.to_string());
                }
                if !snapshot_events.is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let value = session_messages.lock().unwrap().clone();
                write_json(&mut stream, 200, &value).await;
            }
            ("GET", "/permission") => {
                let value = pending_permissions.lock().unwrap().clone();
                write_json(&mut stream, 200, &value).await;
            }
            ("GET", "/question") => {
                let value = pending_questions.lock().unwrap().clone();
                write_json(&mut stream, 200, &value).await;
            }
            ("GET", "/event") => {
                let header = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                if stream.write_all(header).await.is_err()
                    || stream
                        .write_all(b"data: {\"type\":\"server.connected\",\"properties\":{}}\n\n")
                        .await
                        .is_err()
                {
                    return;
                }
                sse_connected.store(true, Ordering::SeqCst);
                let mut receiver = events.subscribe();
                while let Ok(event) = receiver.recv().await {
                    if event == "__close_sse__" {
                        return;
                    }
                    let frame = format!("data: {event}\n\n");
                    if stream.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                }
            }
            ("POST", "/session/ses_test/prompt_async") => {
                if !sse_connected.load(Ordering::SeqCst) {
                    write_response(&mut stream, 409, "text/plain", b"").await;
                    return;
                }
                write_response(&mut stream, 204, "text/plain", b"").await;
                let input = serde_json::from_slice::<serde_json::Value>(&request.body)
                    .ok()
                    .and_then(|value| {
                        value
                            .pointer("/parts/0/text")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                if input == "stale idle" {
                    let _ = events.send(
                        json!({
                            "type": "session.status",
                            "properties": {"sessionID": "ses_test", "status": {"type": "idle"}}
                        })
                        .to_string(),
                    );
                    return;
                }
                if input == "stale busy idle" {
                    for status in ["busy", "idle"] {
                        let _ = events.send(
                            json!({
                                "type": "session.status",
                                "properties": {"sessionID": "ses_test", "status": {"type": status}}
                            })
                            .to_string(),
                        );
                    }
                    return;
                }
                if input == "delta before busy" {
                    let _ = events.send(
                        json!({
                            "id": "evt_message_delta_first",
                            "type": "message.updated",
                            "properties": {
                                "info": {"id": "msg_delta_first", "sessionID": "ses_test", "role": "assistant"}
                            }
                        })
                        .to_string(),
                    );
                    let _ = events.send(
                        json!({
                            "id": "evt_part_delta_first_start",
                            "type": "message.part.updated",
                            "properties": {
                                "sessionID": "ses_test",
                                "part": {
                                    "id": "part_delta_first", "sessionID": "ses_test",
                                    "messageID": "msg_delta_first", "type": "text", "text": ""
                                },
                                "time": 1
                            }
                        })
                        .to_string(),
                    );
                    let _ = events.send(
                        json!({
                            "id": "evt_part_delta_first_delta",
                            "type": "message.part.delta",
                            "properties": {
                                "sessionID": "ses_test", "messageID": "msg_delta_first",
                                "partID": "part_delta_first", "field": "text", "delta": "first"
                            }
                        })
                        .to_string(),
                    );
                    let _ = events.send(
                        json!({
                            "type": "session.status",
                            "properties": {"sessionID": "ses_test", "status": {"type": "busy"}}
                        })
                        .to_string(),
                    );
                    let _ = events.send(
                        json!({
                            "type": "session.status",
                            "properties": {"sessionID": "ses_test", "status": {"type": "idle"}}
                        })
                        .to_string(),
                    );
                    return;
                }
                let _ = events.send(
                    json!({
                        "id": "evt_user_message",
                        "type": "message.updated",
                        "properties": {
                            "info": {"id": "msg_user", "sessionID": "ses_test", "role": "user"}
                        }
                    })
                    .to_string(),
                );
                let _ = events.send(
                    json!({
                        "id": "evt_user_part",
                        "type": "message.part.updated",
                        "properties": {
                            "sessionID": "ses_test",
                            "part": {
                                "id": "part_user",
                                "sessionID": "ses_test",
                                "messageID": "msg_user",
                                "type": "text",
                                "text": "say hello"
                            },
                            "time": 1
                        }
                    })
                    .to_string(),
                );
                let _ = events.send(
                    json!({
                        "type": "session.status",
                        "properties": {"sessionID": "ses_test", "status": {"type": "busy"}}
                    })
                    .to_string(),
                );
                let hold = input == "hold";
                if hold {
                    return;
                }
                if input == "old messages" {
                    let _ = events.send(
                        json!({
                            "type": "message.updated",
                            "properties": {
                                "info": {"id": "msg_history", "sessionID": "ses_test", "role": "assistant"}
                            }
                        })
                        .to_string(),
                    );
                    let _ = events.send(
                        json!({
                            "id": "evt_history_stale_delta",
                            "type": "message.part.delta",
                            "properties": {
                                "sessionID": "ses_test", "messageID": "msg_history",
                                "partID": "part_history", "field": "text", "delta": " leaked"
                            }
                        })
                        .to_string(),
                    );
                }
                let _ = events.send(
                    json!({
                        "id": "evt_assistant_message",
                        "type": "message.updated",
                        "properties": {
                            "info": {"id": "msg_1", "sessionID": "ses_test", "role": "assistant"}
                        }
                    })
                    .to_string(),
                );
                let _ = events.send(
                    json!({
                        "id": "evt_assistant_part_start",
                        "type": "message.part.updated",
                        "properties": {
                            "sessionID": "ses_test",
                            "part": {
                                "id": "part_1",
                                "sessionID": "ses_test",
                                "messageID": "msg_1",
                                "type": "text",
                                "text": ""
                            },
                            "time": 1
                        }
                    })
                    .to_string(),
                );
                let _ = events.send(
                    json!({
                        "id": "evt_assistant_part_delta",
                        "type": "message.part.delta",
                        "properties": {
                            "sessionID": "ses_test", "messageID": "msg_1",
                            "partID": "part_1", "field": "text", "delta": "hello"
                        }
                    })
                    .to_string(),
                );
                let _ = events.send(
                    json!({
                        "id": "evt_assistant_part_end",
                        "type": "message.part.updated",
                        "properties": {
                            "sessionID": "ses_test",
                            "part": {
                                "id": "part_1", "sessionID": "ses_test",
                                "messageID": "msg_1", "type": "text", "text": "hello"
                            },
                            "time": 2
                        }
                    })
                    .to_string(),
                );
                let _ = events.send(
                    json!({
                        "type": "session.status",
                        "properties": {"sessionID": "ses_test", "status": {"type": "idle"}}
                    })
                    .to_string(),
                );
            }
            ("POST", "/session/ses_test/abort") => {
                match abort_response_mode.load(Ordering::SeqCst) {
                    1 => return,
                    2 => {
                        write_response(&mut stream, 500, "application/json", b"false").await;
                        return;
                    }
                    3 => {
                        write_json(&mut stream, 200, &json!(false)).await;
                        return;
                    }
                    _ => {}
                }
                write_json(&mut stream, 200, &json!(true)).await;
                let _ = events.send(
                    json!({
                        "type": "session.status",
                        "properties": {"sessionID": "ses_test", "status": {"type": "idle"}}
                    })
                    .to_string(),
                );
            }
            ("POST", "/permission/per_1/reply") => {
                write_json(&mut stream, 200, &json!(true)).await;
                let _ = events.send(
                    json!({
                        "id": "evt_permission_replied_1",
                        "type": "permission.replied",
                        "properties": {
                            "sessionID": "ses_test",
                            "requestID": "per_1",
                            "reply": "always"
                        }
                    })
                    .to_string(),
                );
            }
            ("POST", "/permission/per_drop/reply") => {}
            _ => write_response(&mut stream, 404, "text/plain", b"").await,
        }
    }

    async fn read_request(stream: &mut TcpStream) -> Option<RequestAudit> {
        const MAX_REQUEST: usize = 64 * 1024;
        let mut bytes = Vec::new();
        let header_end = loop {
            if bytes.len() >= MAX_REQUEST {
                return None;
            }
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.ok()?;
            if count == 0 {
                return None;
            }
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let header = std::str::from_utf8(&bytes[..header_end]).ok()?;
        let mut lines = header.split("\r\n");
        let mut request_line = lines.next()?.split_whitespace();
        let method = request_line.next()?.to_owned();
        let target = request_line.next()?.to_owned();
        let mut content_length = 0;
        let mut authorization = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok()?;
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
        if header_end + content_length > MAX_REQUEST {
            return None;
        }
        while bytes.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.ok()?;
            if count == 0 {
                return None;
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        Some(RequestAudit {
            method,
            target,
            authorization,
            body: bytes[header_end..header_end + content_length].to_vec(),
        })
    }

    async fn write_json(stream: &mut TcpStream, status: u16, value: &serde_json::Value) {
        write_response(
            stream,
            status,
            "application/json",
            value.to_string().as_bytes(),
        )
        .await;
    }

    async fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
        let reason = match status {
            200 => "OK",
            204 => "No Content",
            401 => "Unauthorized",
            404 => "Not Found",
            409 => "Conflict",
            _ => "Error",
        };
        let header = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes()).await;
        let _ = stream.write_all(body).await;
    }

    fn required_doc() -> serde_json::Value {
        // Exact path parameter names and methods from packages/sdk/openapi.json at
        // opencode v1.18.3 (127bdb30784d508cc556c71a0f32b508a3061517).
        json!({
            "paths": {
                "/event": {"get": {}},
                "/session": {"post": {}},
                "/session/status": {"get": {}},
                "/session/{sessionID}": {"get": {}},
                "/session/{sessionID}/message": {"get": {}},
                "/session/{sessionID}/prompt_async": {"post": {}},
                "/session/{sessionID}/abort": {"post": {}},
                "/permission": {"get": {}},
                "/permission/{requestID}/reply": {"post": {}},
                "/question": {"get": {}}
            }
        })
    }

    async fn next_event(events: &mut mpsc::Receiver<ProviderEvent>) -> ProviderEvent {
        tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("provider event timeout")
            .expect("provider event channel closed")
    }

    #[test]
    fn server_contract_accepts_only_the_tested_version_and_required_routes() {
        let health = json!({"healthy": true, "version": "1.18.3"});
        let doc = required_doc();

        assert!(super::validate_server_contract(&health, &doc).is_ok());
        assert!(super::validate_server_contract(
            &json!({"healthy": true, "version": "1.18.2"}),
            &doc
        )
        .is_err());

        let mut missing_permission = doc.clone();
        missing_permission
            .pointer_mut("/paths")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("/permission");
        assert!(super::validate_server_contract(&health, &missing_permission).is_err());

        let mut legacy_parameter_name = doc.clone();
        let paths = legacy_parameter_name
            .pointer_mut("/paths")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        paths.remove("/session/{sessionID}");
        paths.insert("/session/{id}".to_owned(), json!({"get": {}}));
        assert!(super::validate_server_contract(&health, &legacy_parameter_name).is_err());
    }

    #[test]
    fn spawn_contract_uses_ephemeral_loopback_without_unsafe_flags() {
        let arguments = super::command_arguments();
        assert_eq!(
            arguments,
            [
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                "0",
                "--no-mdns"
            ]
        );
        assert!(!arguments.iter().any(|argument| argument.contains("auto")));
        assert_eq!(
            super::parse_listening_url(b"opencode server listening on http://127.0.0.1:43129\n")
                .as_deref(),
            Some("http://127.0.0.1:43129")
        );
        assert!(
            super::parse_listening_url(b"opencode server listening on http://0.0.0.0:43129\n")
                .is_none()
        );
        assert!(
            super::parse_listening_url(b"opencode server listening on http://127.0.0.1:0\n")
                .is_none()
        );
    }

    #[test]
    fn spawn_passwords_are_csprng_and_not_reused() {
        let first = super::generate_password().unwrap();
        let second = super::generate_password().unwrap();
        assert!(first.len() >= 40);
        assert_ne!(first, second);
        assert!(!first.contains(':'));
    }

    #[test]
    fn read_only_mode_denies_mutating_tools_without_relaxing_write_confirmed_config() {
        assert_eq!(
            super::permission_override(SandboxAccess::ReadOnly),
            Some(r#"{"edit":"deny","bash":"deny","external_directory":"deny"}"#)
        );
        assert_eq!(
            super::permission_override(SandboxAccess::WorkspaceWriteConfirmed),
            None
        );
    }

    #[test]
    fn permission_decisions_use_only_the_documented_wire_values() {
        assert_eq!(
            super::permission_response_value(&ProviderResponse::Approve),
            Some("once")
        );
        assert_eq!(
            super::permission_response_value(&ProviderResponse::ApproveForSession),
            Some("always")
        );
        assert_eq!(
            super::permission_response_value(&ProviderResponse::Decline),
            Some("reject")
        );
        assert_eq!(
            super::permission_response_value(&ProviderResponse::Answers(Default::default())),
            None
        );
    }

    #[tokio::test]
    async fn managed_lifecycle_opens_authenticated_sse_before_prompt_and_filters_output() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );

        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-1".to_owned(),
                cwd: PathBuf::from("/tmp/project with spaces"),
                resume_session_id: None,
                initial_input: "say hello".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready {
                run_id: "run-1".to_owned(),
                session_id: "ses_test".to_owned(),
            }
        );
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working {
                run_id: "run-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            }
        );
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::OutputDelta {
                run_id: "run-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                text: "hello".to_owned(),
            }
        );
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TurnCompleted {
                run_id: "run-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                outcome: TurnOutcome::Completed,
            }
        );

        let audits = server.audits();
        assert!(audits.iter().all(|request| request.authorization.is_some()));
        let prompt = audits
            .iter()
            .find(|request| request.target.starts_with("/session/ses_test/prompt_async"))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&prompt.body).unwrap(),
            json!({"parts": [{"type": "text", "text": "say hello"}]})
        );
        assert!(server.sse_connected.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stale_idle_after_prompt_does_not_complete_an_unarmed_turn() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-stale-idle".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "stale idle".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
                .await
                .is_err()
        );

        server.emit(json!({
            "type": "session.status",
            "properties": {"sessionID": "ses_test", "status": {"type": "busy"}}
        }));
        server.emit(json!({
            "id": "evt_after_idle_message",
            "type": "message.updated",
            "properties": {
                "info": {"id": "msg_after_idle", "sessionID": "ses_test", "role": "assistant"}
            }
        }));
        server.emit(json!({
            "id": "evt_after_idle_part_start",
            "type": "message.part.updated",
            "properties": {
                "sessionID": "ses_test",
                "part": {
                    "id": "part_after_idle", "sessionID": "ses_test",
                    "messageID": "msg_after_idle", "type": "text", "text": ""
                },
                "time": 1
            }
        }));
        server.emit(json!({
            "id": "evt_after_idle_part_delta",
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test", "messageID": "msg_after_idle",
                "partID": "part_after_idle", "field": "text", "delta": "ok"
            }
        }));
        server.emit(json!({
            "type": "session.status",
            "properties": {"sessionID": "ses_test", "status": {"type": "idle"}}
        }));

        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::OutputDelta { ref text, .. } if text == "ok"
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::TurnCompleted {
                outcome: TurnOutcome::Completed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn stale_busy_idle_pair_does_not_arm_or_complete_a_new_turn() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-stale-status-pair".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "stale busy idle".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn assistant_delta_before_busy_emits_working_before_output() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-delta-first".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "delta before busy".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::OutputDelta { ref text, .. } if text == "first"
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::TurnCompleted {
                outcome: TurnOutcome::Completed,
                ..
            }
        ));
    }

    fn actor_with_unarmed_turn(event_tx: mpsc::Sender<ProviderEvent>) -> super::Actor {
        let mut actor = super::Actor::connected(
            "http://127.0.0.1:1".to_owned(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            event_tx,
        );
        actor.run_id = Some("run-stream-unit".to_owned());
        actor.session_id = Some("ses_test".to_owned());
        actor.current_turn = Some(super::CurrentTurn {
            id: "turn-1".to_owned(),
            armed: false,
            working_emitted: false,
            interrupt_requested: false,
            assistant_messages: Default::default(),
            output_bytes: 0,
            deadline: tokio::time::Instant::now() + super::TURN_IDLE_TIMEOUT,
        });
        actor
    }

    async fn prime_assistant_part(
        actor: &mut super::Actor,
        event_rx: &mut mpsc::Receiver<ProviderEvent>,
    ) {
        assert!(actor
            .handle_event(&json!({
                "id": "evt_prime_message",
                "type": "message.updated",
                "properties": {"info": {
                    "id": "msg_prime", "sessionID": "ses_test", "role": "assistant"
                }}
            }))
            .await
            .is_ok());
        assert!(matches!(
            next_event(event_rx).await,
            ProviderEvent::Working { .. }
        ));
        assert!(actor
            .handle_event(&json!({
                "id": "evt_prime_start",
                "type": "message.part.updated",
                "properties": {
                    "sessionID": "ses_test",
                    "part": {
                        "id": "part_prime", "sessionID": "ses_test",
                        "messageID": "msg_prime", "type": "text", "text": ""
                    },
                    "time": 1
                }
            }))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn delta_events_are_deduplicated_owner_checked_and_losslessly_chunked() {
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let mut actor = actor_with_unarmed_turn(event_tx);
        prime_assistant_part(&mut actor, &mut event_rx).await;
        let delta = json!({
            "id": "evt_prime_delta",
            "type": "message.part.delta",
            "properties": {
                "sessionID": "ses_test", "messageID": "msg_prime",
                "partID": "part_prime", "field": "text", "delta": "once"
            }
        });
        assert!(actor.handle_event(&delta).await.is_ok());
        assert!(actor.handle_event(&delta).await.is_ok());
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::OutputDelta { ref text, .. } if text == "once"
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(actor
            .handle_event(&json!({
                "id": "evt_bad_owner",
                "type": "message.part.delta",
                "properties": {
                    "sessionID": "ses_test", "messageID": "msg_prime",
                    "partID": "part_unknown", "field": "text", "delta": "bad"
                }
            }))
            .await
            .is_err());

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let mut actor = actor_with_unarmed_turn(event_tx);
        prime_assistant_part(&mut actor, &mut event_rx).await;
        let expected = "x".repeat(super::MAX_VISIBLE_TEXT_BYTES + 17);
        assert!(actor
            .handle_event(&json!({
                "id": "evt_large_delta",
                "type": "message.part.delta",
                "properties": {
                    "sessionID": "ses_test", "messageID": "msg_prime",
                    "partID": "part_prime", "field": "text", "delta": expected
                }
            }))
            .await
            .is_ok());
        let mut actual = String::new();
        for _ in 0..2 {
            match next_event(&mut event_rx).await {
                ProviderEvent::OutputDelta { text, .. } => {
                    assert!(text.len() <= super::MAX_VISIBLE_TEXT_BYTES);
                    actual.push_str(&text);
                }
                other => panic!("unexpected chunk event: {other:?}"),
            }
        }
        assert_eq!(actual, "x".repeat(super::MAX_VISIBLE_TEXT_BYTES + 17));
    }

    #[tokio::test]
    async fn baseline_messages_are_not_replayed_as_new_turn_output() {
        let server = FakeServer::start().await;
        server.set_messages(json!([{
            "info": {"id": "msg_history", "sessionID": "ses_test", "role": "assistant"},
            "parts": [{
                "id": "part_history", "sessionID": "ses_test",
                "messageID": "msg_history", "type": "text", "text": "old"
            }]
        }]));
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-old-message".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "old messages".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::OutputDelta { ref text, .. } if text == "hello"
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::TurnCompleted {
                outcome: TurnOutcome::Completed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn permission_is_session_filtered_and_resolves_only_after_provider_event() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-permission".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: String::new(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));

        server.emit(json!({
            "id": "evt_permission_other",
            "type": "permission.asked",
            "properties": {
                "id": "per_other",
                "sessionID": "ses_other",
                "permission": "bash",
                "patterns": ["must be ignored"],
                "metadata": {},
                "always": [],
                "tool": {"messageID": "msg_other", "callID": "call_other"}
            }
        }));
        server.emit(json!({
            "id": "evt_permission_1",
            "type": "permission.asked",
            "properties": {
                "id": "per_1",
                "sessionID": "ses_test",
                "permission": "bash",
                "patterns": ["git status"],
                "metadata": {"secret": "must never be surfaced"},
                "always": ["git *"],
                "tool": {"messageID": "msg_1", "callID": "call_1"}
            }
        }));

        let token = match next_event(&mut event_rx).await {
            ProviderEvent::AttentionRequested { run_id, attention } => {
                assert_eq!(run_id, "run-permission");
                assert_eq!(attention.class, AttentionClass::PermissionApproval);
                assert_eq!(attention.thread_id, "ses_test");
                assert_eq!(attention.item_id, "call_1");
                assert_eq!(attention.requested_action, "bash: git status");
                assert!(!attention.requested_action.contains("secret"));
                attention.token
            }
            other => panic!("unexpected event: {other:?}"),
        };

        command_tx
            .send(ProviderCommand::Respond {
                token,
                response: ProviderResponse::ApproveForSession,
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::ResponseResolved {
                run_id: "run-permission".to_owned(),
                request_id: "string:per_1".to_owned(),
            }
        );
        let permission_response = server
            .audits()
            .into_iter()
            .find(|request| request.target.starts_with("/permission/per_1/reply"))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&permission_response.body).unwrap(),
            json!({"reply": "always"})
        );
    }

    #[tokio::test]
    async fn pending_permission_suspends_the_turn_idle_timeout() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-permission-wait".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "hold".to_owned(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
        server.emit(json!({
            "id": "evt_permission_wait",
            "type": "permission.asked",
            "properties": {
                "id": "per_1", "sessionID": "ses_test", "permission": "bash",
                "patterns": ["git status"], "metadata": {}, "always": [],
                "tool": {"messageID": "msg_wait", "callID": "call_wait"}
            }
        }));
        let token = match next_event(&mut event_rx).await {
            ProviderEvent::AttentionRequested { attention, .. } => attention.token,
            other => panic!("unexpected permission wait event: {other:?}"),
        };
        tokio::time::sleep(super::TURN_IDLE_TIMEOUT + Duration::from_millis(100)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
                .await
                .is_err()
        );
        command_tx
            .send(ProviderCommand::Respond {
                token,
                response: ProviderResponse::ApproveForSession,
            })
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::ResponseResolved { .. }
        ));
    }

    #[tokio::test]
    async fn interrupt_uses_abort_and_completes_as_interrupted_on_idle() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-abort".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "hold".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));

        command_tx.send(ProviderCommand::Interrupt).await.unwrap();
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TurnCompleted {
                run_id: "run-abort".to_owned(),
                turn_id: "turn-1".to_owned(),
                outcome: TurnOutcome::Interrupted,
            }
        );
        assert!(server
            .audits()
            .iter()
            .any(|request| request.target.starts_with("/session/ses_test/abort")));
    }

    #[tokio::test]
    async fn structured_question_is_non_respondable_aborted_and_fails_closed() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-question".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        server.emit(json!({
            "id": "evt_question_1",
            "type": "question.asked",
            "properties": {
                "id": "que_1",
                "sessionID": "ses_test",
                "questions": [{"header": "Secret", "question": "Do not persist me"}]
            }
        }));

        match next_event(&mut event_rx).await {
            ProviderEvent::AttentionRequested { attention, .. } => {
                assert_eq!(attention.class, AttentionClass::UserInput);
                assert_eq!(attention.item_id, "que_1");
                assert!(!attention.requested_action.contains("Do not persist me"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-question".to_owned(),
                reason: TransportFailure::Protocol,
            }
        );
        assert!(server
            .audits()
            .iter()
            .any(|request| request.target.starts_with("/session/ses_test/abort")));
    }

    #[tokio::test]
    async fn ambiguous_question_abort_is_reported_as_delivery_unknown() {
        let server = FakeServer::start().await;
        server.drop_abort_response();
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-question-abort-unknown".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        server.emit(json!({
            "id": "evt_question_abort_unknown",
            "type": "question.asked",
            "properties": {
                "id": "que_abort_unknown",
                "sessionID": "ses_test",
                "questions": [{"header": "Confirm", "question": "Continue?"}]
            }
        }));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::AttentionRequested { .. }
        ));
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-question-abort-unknown".to_owned(),
                reason: TransportFailure::DeliveryUnknown,
            }
        );
    }

    #[tokio::test]
    async fn definite_question_abort_rejections_are_protocol_failures() {
        for http_error in [true, false] {
            let server = FakeServer::start().await;
            if http_error {
                server.reject_abort_with_http_error();
            } else {
                server.reject_abort_with_false();
            }
            let run_id = if http_error {
                "run-question-abort-http"
            } else {
                "run-question-abort-false"
            };
            let (command_tx, command_rx) = mpsc::channel(8);
            let (event_tx, mut event_rx) = mpsc::channel(8);
            super::spawn_for_test(
                server.base_url.clone(),
                USERNAME.to_owned(),
                PASSWORD.to_owned(),
                command_rx,
                event_tx,
            );
            command_tx
                .send(ProviderCommand::StartOrResume(StartOrResume {
                    run_id: run_id.to_owned(),
                    cwd: PathBuf::from("/tmp/project"),
                    resume_session_id: None,
                    initial_input: String::new(),
                    sandbox: SandboxAccess::ReadOnly,
                }))
                .await
                .unwrap();
            assert!(matches!(
                next_event(&mut event_rx).await,
                ProviderEvent::Ready { .. }
            ));
            server.emit(json!({
                "id": format!("evt_{run_id}"),
                "type": "question.asked",
                "properties": {
                    "id": format!("que_{run_id}"),
                    "sessionID": "ses_test",
                    "questions": [{"header": "Confirm", "question": "Continue?"}]
                }
            }));
            assert!(matches!(
                next_event(&mut event_rx).await,
                ProviderEvent::AttentionRequested { .. }
            ));
            assert_eq!(
                next_event(&mut event_rx).await,
                ProviderEvent::TransportFailed {
                    run_id: run_id.to_owned(),
                    reason: TransportFailure::Protocol,
                }
            );
        }
    }

    #[tokio::test]
    async fn resume_verifies_the_session_without_creating_or_replaying_a_turn() {
        let server = FakeServer::start().await;
        server.set_pending_permissions(json!([{
            "id": "per_resume",
            "sessionID": "ses_test",
            "permission": "edit",
            "patterns": ["src/main.rs"],
            "metadata": {},
            "always": ["src/**"],
            "tool": {"messageID": "msg_resume", "callID": "call_resume"}
        }]));
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-resume".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready {
                run_id: "run-resume".to_owned(),
                session_id: "ses_test".to_owned(),
            }
        );
        let audits = server.audits();
        assert!(audits
            .iter()
            .any(|request| request.method == "GET"
                && request.target.starts_with("/session/ses_test")));
        assert!(!audits
            .iter()
            .any(|request| request.method == "POST" && request.target.starts_with("/session")));
        let request_position = |path: &str| {
            audits
                .iter()
                .position(|request| request.target.starts_with(path))
                .unwrap_or_else(|| panic!("missing resume request: {path}"))
        };
        let sse = request_position("/event");
        for snapshot in [
            "/session/status",
            "/permission",
            "/question",
            "/session/ses_test/message",
        ] {
            assert!(sse < request_position(snapshot));
        }
        match next_event(&mut event_rx).await {
            ProviderEvent::AttentionRequested { attention, .. } => {
                assert_eq!(attention.class, AttentionClass::PermissionApproval);
                assert_eq!(attention.item_id, "call_resume");
                assert_eq!(attention.requested_action, "edit: src/main.rs");
            }
            other => panic!("unexpected resume event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn pending_question_snapshot_is_surfaced_aborted_and_fails_closed() {
        let server = FakeServer::start().await;
        server.set_pending_questions(json!([{
            "id": "que_resume", "sessionID": "ses_test",
            "questions": [{"header": "Confirm", "question": "Continue?"}],
            "tool": {"messageID": "msg_question", "callID": "call_question"}
        }]));
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-resume-question".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::AttentionRequested { ref attention, .. }
                if attention.item_id == "que_resume"
        ));
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-resume-question".to_owned(),
                reason: TransportFailure::Protocol,
            }
        );
        assert!(server
            .audits()
            .iter()
            .any(|request| request.target.starts_with("/session/ses_test/abort")));
    }

    #[tokio::test]
    async fn resume_rejects_too_many_non_text_parts_in_message_snapshot() {
        let server = FakeServer::start().await;
        let parts = (0..=super::MAX_TRACKED_PARTS)
            .map(|index| {
                json!({
                    "id": format!("part_{index}"),
                    "sessionID": "ses_test",
                    "messageID": "msg_history",
                    "type": "tool"
                })
            })
            .collect::<Vec<_>>();
        server.set_messages(json!([{
            "info": {"id": "msg_history", "sessionID": "ses_test", "role": "assistant"},
            "parts": parts
        }]));
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-snapshot-parts-bound".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-snapshot-parts-bound".to_owned(),
                reason: TransportFailure::Protocol,
            }
        );
    }

    #[tokio::test]
    async fn resume_rejects_a_malformed_present_status_instead_of_assuming_idle() {
        let server = FakeServer::start().await;
        server.set_status(json!({"ses_test": {}}));
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-malformed-status".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-malformed-status".to_owned(),
                reason: TransportFailure::Protocol,
            }
        );
    }

    #[tokio::test]
    async fn resume_takes_a_final_status_cut_after_message_hydration() {
        let server = FakeServer::start().await;
        server.set_status_sequence(vec![
            json!({"ses_test": {"type": "idle"}}),
            json!({"ses_test": {"type": "busy"}}),
        ]);
        server.set_messages(json!([{
            "info": {"id": "msg_resume_busy", "sessionID": "ses_test", "role": "assistant"},
            "parts": [{
                "id": "part_resume_busy", "sessionID": "ses_test",
                "messageID": "msg_resume_busy", "type": "text", "text": ""
            }]
        }]));
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-resume-final-cut".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
        assert_eq!(
            server
                .audits()
                .iter()
                .filter(|request| request.target.starts_with("/session/status"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn delayed_new_message_after_resume_ready_recovers_the_external_turn() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-resume-delayed-event".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        server.emit(json!({
            "id": "evt_delayed_user",
            "type": "message.updated",
            "properties": {
                "info": {"id": "msg_delayed_user", "sessionID": "ses_test", "role": "user"}
            }
        }));

        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
    }

    #[tokio::test]
    async fn resume_fusion_does_not_reapply_deltas_already_in_the_message_snapshot() {
        let server = FakeServer::start().await;
        server.set_messages(json!([{
            "info": {"id": "msg_complete", "sessionID": "ses_test", "role": "assistant"},
            "parts": [{
                "id": "part_complete", "sessionID": "ses_test",
                "messageID": "msg_complete", "type": "text", "text": "hello",
                "time": {"start": 1, "end": 2}
            }]
        }]));
        server.emit_during_next_message_snapshot(vec![
            json!({
                "id": "evt_complete_start",
                "type": "message.part.updated",
                "properties": {
                    "sessionID": "ses_test",
                    "part": {
                        "id": "part_complete", "sessionID": "ses_test",
                        "messageID": "msg_complete", "type": "text", "text": "",
                        "time": {"start": 1}
                    },
                    "time": 1
                }
            }),
            json!({
                "id": "evt_complete_delta",
                "type": "message.part.delta",
                "properties": {
                    "sessionID": "ses_test", "messageID": "msg_complete",
                    "partID": "part_complete", "field": "text", "delta": "hello"
                }
            }),
            json!({
                "id": "evt_complete_end",
                "type": "message.part.updated",
                "properties": {
                    "sessionID": "ses_test",
                    "part": {
                        "id": "part_complete", "sessionID": "ses_test",
                        "messageID": "msg_complete", "type": "text", "text": "hello",
                        "time": {"start": 1, "end": 2}
                    },
                    "time": 2
                }
            }),
        ]);
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-resume-idempotent-delta".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready {
                run_id: "run-resume-idempotent-delta".to_owned(),
                session_id: "ses_test".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn authentication_failure_fails_before_session_creation() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            "wrong-password".to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-auth".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-auth".to_owned(),
                reason: TransportFailure::Protocol,
            }
        );
        assert!(!server
            .audits()
            .iter()
            .any(|request| request.target.starts_with("/session")));
    }

    #[tokio::test]
    async fn provider_session_error_finishes_the_current_turn_as_failed() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-error".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "hold".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
        server.emit(json!({
            "type": "session.error",
            "properties": {"sessionID": "ses_test", "error": {"name": "UnknownError"}}
        }));
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TurnCompleted {
                run_id: "run-error".to_owned(),
                turn_id: "turn-1".to_owned(),
                outcome: TurnOutcome::Failed,
            }
        );
    }

    #[tokio::test]
    async fn malformed_or_oversized_sse_frame_fails_closed() {
        for payload in [
            "{not-json".to_owned(),
            "x".repeat(super::MAX_PROVIDER_FRAME_BYTES + 1),
        ] {
            let server = FakeServer::start().await;
            let (command_tx, command_rx) = mpsc::channel(8);
            let (event_tx, mut event_rx) = mpsc::channel(8);
            super::spawn_for_test(
                server.base_url.clone(),
                USERNAME.to_owned(),
                PASSWORD.to_owned(),
                command_rx,
                event_tx,
            );
            command_tx
                .send(ProviderCommand::StartOrResume(StartOrResume {
                    run_id: "run-overflow".to_owned(),
                    cwd: PathBuf::from("/tmp/project"),
                    resume_session_id: None,
                    initial_input: String::new(),
                    sandbox: SandboxAccess::ReadOnly,
                }))
                .await
                .unwrap();
            assert!(matches!(
                next_event(&mut event_rx).await,
                ProviderEvent::Ready { .. }
            ));
            server.emit_raw(payload);
            assert_eq!(
                next_event(&mut event_rx).await,
                ProviderEvent::TransportFailed {
                    run_id: "run-overflow".to_owned(),
                    reason: TransportFailure::Protocol,
                }
            );
        }
    }

    #[tokio::test]
    async fn turn_without_terminal_event_times_out() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-timeout".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "hold".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-timeout".to_owned(),
                reason: TransportFailure::Timeout,
            }
        );
    }

    #[tokio::test]
    async fn ambiguous_permission_delivery_is_never_replayed() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-unknown".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: String::new(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        server.emit(json!({
            "id": "evt_permission_drop",
            "type": "permission.asked",
            "properties": {
                "id": "per_drop", "sessionID": "ses_test", "permission": "bash",
                "patterns": ["git status"], "metadata": {}, "always": [],
                "tool": {"messageID": "msg_1", "callID": "call_drop"}
            }
        }));
        let token = match next_event(&mut event_rx).await {
            ProviderEvent::AttentionRequested { attention, .. } => attention.token,
            other => panic!("unexpected event: {other:?}"),
        };
        command_tx
            .send(ProviderCommand::Respond {
                token,
                response: ProviderResponse::Approve,
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-unknown".to_owned(),
                reason: TransportFailure::DeliveryUnknown,
            }
        );
        assert_eq!(
            server
                .audits()
                .iter()
                .filter(|request| request.target.starts_with("/permission/per_drop/reply"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn sse_eof_during_an_active_turn_is_delivery_unknown() {
        let server = FakeServer::start().await;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-eof".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: None,
                initial_input: "hold".to_owned(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Working { .. }
        ));
        server.close_sse();
        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-eof".to_owned(),
                reason: TransportFailure::DeliveryUnknown,
            }
        );
    }

    fn historical_actor(event_tx: mpsc::Sender<ProviderEvent>) -> super::Actor {
        let mut actor = super::Actor::connected(
            "http://127.0.0.1:1".to_owned(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            event_tx,
        );
        actor.run_id = Some("run-history-budget".to_owned());
        actor.session_id = Some("ses_test".to_owned());
        actor
    }

    fn assistant_part_updated(id: &str, text: String) -> serde_json::Value {
        json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": "ses_test",
                "part": {
                    "id": id,
                    "sessionID": "ses_test",
                    "messageID": "msg_history_budget",
                    "type": "text",
                    "text": text
                },
                "time": 1
            }
        })
    }

    #[tokio::test]
    async fn historical_part_bytes_are_cumulative_and_removals_release_budget() {
        const EXPECTED_SESSION_BUDGET: usize = 32 * 1024 * 1024;

        let (event_tx, _event_rx) = mpsc::channel(4);
        let mut actor = historical_actor(event_tx);
        assert!(actor
            .handle_event(&json!({
                "type": "message.updated",
                "properties": {"info": {
                    "id": "msg_history_budget",
                    "sessionID": "ses_test",
                    "role": "assistant"
                }}
            }))
            .await
            .is_ok());

        for index in 0..31 {
            assert!(actor
                .handle_event(&assistant_part_updated(
                    &format!("part_history_{index}"),
                    "x".repeat(super::MAX_PROVIDER_FRAME_BYTES),
                ))
                .await
                .is_ok());
        }
        assert!(actor
            .handle_event(&assistant_part_updated(
                "part_history_tail",
                "y".repeat(super::MAX_PROVIDER_FRAME_BYTES / 2),
            ))
            .await
            .is_ok());
        assert!(actor
            .handle_event(&assistant_part_updated(
                "part_history_tail",
                "y".repeat(super::MAX_PROVIDER_FRAME_BYTES),
            ))
            .await
            .is_ok());
        assert_eq!(
            actor.parts.values().map(String::len).sum::<usize>(),
            EXPECTED_SESSION_BUDGET
        );
        assert_eq!(actor.parts_bytes, EXPECTED_SESSION_BUDGET);

        assert!(actor
            .handle_event(&assistant_part_updated("part_overflow", "z".to_owned()))
            .await
            .is_err());
        assert!(!actor.parts.contains_key("part_overflow"));

        assert!(actor
            .handle_event(&json!({
                "type": "message.part.removed",
                "properties": {
                    "sessionID": "ses_test",
                    "messageID": "msg_history_budget",
                    "partID": "part_history_0"
                }
            }))
            .await
            .is_ok());
        assert!(actor
            .handle_event(&assistant_part_updated(
                "part_replacement",
                "r".repeat(super::MAX_PROVIDER_FRAME_BYTES),
            ))
            .await
            .is_ok());
        assert_eq!(
            actor.parts.values().map(String::len).sum::<usize>(),
            EXPECTED_SESSION_BUDGET
        );
        assert_eq!(actor.parts_bytes, EXPECTED_SESSION_BUDGET);
    }

    fn permission_asked(id: &str) -> serde_json::Value {
        json!({
            "type": "permission.asked",
            "properties": {
                "id": id,
                "sessionID": "ses_test",
                "permission": "bash",
                "patterns": ["git status"],
                "metadata": {},
                "always": []
            }
        })
    }

    #[tokio::test]
    async fn live_pending_permissions_are_bounded_deduplicated_and_reusable_after_resolution() {
        const EXPECTED_PENDING_LIMIT: usize = 128;

        let (event_tx, mut event_rx) = mpsc::channel(EXPECTED_PENDING_LIMIT + 2);
        let mut actor = historical_actor(event_tx);
        for index in 0..EXPECTED_PENDING_LIMIT {
            assert!(actor
                .handle_event(&permission_asked(&format!("per_{index}")))
                .await
                .is_ok());
        }
        assert_eq!(actor.pending_permissions.len(), EXPECTED_PENDING_LIMIT);
        for _ in 0..EXPECTED_PENDING_LIMIT {
            assert!(matches!(
                event_rx.try_recv(),
                Ok(ProviderEvent::AttentionRequested { .. })
            ));
        }

        assert!(actor
            .handle_event(&permission_asked("per_overflow"))
            .await
            .is_err());
        assert!(event_rx.try_recv().is_err());
        assert_eq!(actor.pending_permissions.len(), EXPECTED_PENDING_LIMIT);

        assert!(actor.handle_event(&permission_asked("per_0")).await.is_ok());
        assert!(event_rx.try_recv().is_err());

        assert!(actor
            .handle_event(&json!({
                "type": "permission.replied",
                "properties": {
                    "sessionID": "ses_test",
                    "requestID": "per_0",
                    "reply": "reject"
                }
            }))
            .await
            .is_ok());
        assert!(matches!(
            event_rx.try_recv(),
            Ok(ProviderEvent::ResponseResolved { .. })
        ));
        assert!(actor
            .handle_event(&permission_asked("per_after_resolve"))
            .await
            .is_ok());
        assert_eq!(actor.pending_permissions.len(), EXPECTED_PENDING_LIMIT);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(ProviderEvent::AttentionRequested { .. })
        ));
    }

    #[tokio::test]
    async fn permission_snapshot_uses_the_dedicated_pending_limit() {
        let server = FakeServer::start().await;
        server.set_pending_permissions(serde_json::Value::Array(
            (0..=128)
                .map(|index| {
                    json!({
                        "id": format!("per_snapshot_{index}"),
                        "sessionID": "ses_test",
                        "permission": "bash",
                        "patterns": ["git status"],
                        "metadata": {},
                        "always": []
                    })
                })
                .collect(),
        ));
        let (command_tx, command_rx) = mpsc::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(2);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-permission-snapshot-limit".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert_eq!(
            next_event(&mut event_rx).await,
            ProviderEvent::TransportFailed {
                run_id: "run-permission-snapshot-limit".to_owned(),
                reason: TransportFailure::Protocol,
            }
        );
    }

    #[tokio::test]
    async fn permission_snapshot_cap_counts_only_unique_permissions_for_the_resumed_session() {
        let server = FakeServer::start().await;
        let mut permissions = (0..=128)
            .map(|index| {
                json!({
                    "id": format!("per_other_{index}"),
                    "sessionID": "ses_other",
                    "permission": "bash",
                    "patterns": ["git status"],
                    "metadata": {},
                    "always": []
                })
            })
            .collect::<Vec<_>>();
        permissions.extend((0..=128).map(|_| {
            json!({
                "id": "per_resume_unique",
                "sessionID": "ses_test",
                "permission": "edit",
                "patterns": ["src/main.rs"],
                "metadata": {},
                "always": []
            })
        }));
        server.set_pending_permissions(serde_json::Value::Array(permissions));
        let (command_tx, command_rx) = mpsc::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(4);
        super::spawn_for_test(
            server.base_url.clone(),
            USERNAME.to_owned(),
            PASSWORD.to_owned(),
            command_rx,
            event_tx,
        );
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-permission-snapshot-filtered-limit".to_owned(),
                cwd: PathBuf::from("/tmp/project"),
                resume_session_id: Some("ses_test".to_owned()),
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::Ready { .. }
        ));
        assert!(matches!(
            next_event(&mut event_rx).await,
            ProviderEvent::AttentionRequested { ref attention, .. }
                if attention.item_id == "per_resume_unique"
        ));
    }

    async fn raw_sse_messages(writes: Vec<Vec<u8>>) -> Vec<super::SseMessage> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for write in writes {
                stream.write_all(&write).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let (sender, mut receiver) = mpsc::channel(16);
        super::read_sse(response, sender).await;
        let mut messages = Vec::new();
        while let Some(message) = receiver.recv().await {
            let terminal = matches!(
                message,
                super::SseMessage::Closed | super::SseMessage::Invalid
            );
            messages.push(message);
            if terminal {
                break;
            }
        }
        messages
    }

    #[tokio::test]
    async fn sse_limits_each_frame_not_the_total_http_chunk() {
        let padding = "x".repeat(super::MAX_PROVIDER_FRAME_BYTES / 2);
        let first = format!("data: {{\"type\":\"one\",\"padding\":\"{padding}\"}}\n\n");
        let second = format!("data: {{\"type\":\"two\",\"padding\":\"{padding}\"}}\n\n");
        let combined = [first.as_bytes(), second.as_bytes()].concat();
        assert!(combined.len() > super::MAX_PROVIDER_FRAME_BYTES);
        let mut decoder = super::SseDecoder::default();
        let decoded = decoder
            .push_chunk(&combined)
            .expect("the limit applies to each frame, not to the HTTP chunk");
        assert_eq!(decoded.len(), 2);
        assert!(decoder.finish().is_ok());

        let messages = raw_sse_messages(vec![combined]).await;
        assert!(matches!(
            messages.as_slice(),
            [super::SseMessage::Event(first), super::SseMessage::Event(second), super::SseMessage::Closed]
                if first.get("type").and_then(serde_json::Value::as_str) == Some("one")
                    && second.get("type").and_then(serde_json::Value::as_str) == Some("two")
        ));
    }

    #[tokio::test]
    async fn sse_rejects_one_oversized_frame_and_handles_crlf_splits_and_eof() {
        let oversized = format!(
            "data: {{\"type\":\"large\",\"padding\":\"{}\"}}\n\n",
            "x".repeat(super::MAX_PROVIDER_FRAME_BYTES)
        );
        let mut decoder = super::SseDecoder::default();
        assert!(decoder.push_chunk(oversized.as_bytes()).is_err());
        let messages = raw_sse_messages(vec![oversized.into_bytes()]).await;
        assert!(matches!(messages.as_slice(), [super::SseMessage::Invalid]));

        let messages = raw_sse_messages(vec![
            b"da".to_vec(),
            b"ta: {\"type\":\"split\"}\r".to_vec(),
            b"\n\r".to_vec(),
            b"\n".to_vec(),
        ])
        .await;
        assert!(matches!(
            messages.as_slice(),
            [super::SseMessage::Event(value), super::SseMessage::Closed]
                if value.get("type").and_then(serde_json::Value::as_str) == Some("split")
        ));

        let messages = raw_sse_messages(vec![b"data: {\"type\":\"truncated\"}".to_vec()]).await;
        assert!(matches!(messages.as_slice(), [super::SseMessage::Invalid]));
    }
}
