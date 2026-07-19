use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::{json, Map, Value};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::mpsc,
};

use super::{
    AttentionClass, ProviderAttention, ProviderCommand, ProviderEvent, ProviderQuestion,
    ProviderQuestionOption, ProviderResponse, ResponseToken, RpcId, StartOrResume,
    TransportFailure, TurnOutcome,
};

const MAX_PROVIDER_FRAME_BYTES: usize = 1024 * 1024;
const MAX_VISIBLE_TEXT_BYTES: usize = 16 * 1024;
pub(crate) const TESTED_VERSION: &str = "2.1.212";
const MAX_IDENTIFIER_BYTES: usize = 1024;
const MAX_QUESTION_BYTES: usize = 4 * 1024;
const MAX_ANSWER_BYTES: usize = 4 * 1024;
const STDERR_DRAIN_BUFFER_BYTES: usize = 4 * 1024;
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(test))]
const TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
#[cfg(test)]
const TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

const PERMISSION_METHOD: &str = "claude/control/can_use_tool";
const QUESTION_METHOD: &str = "claude/control/ask_user_question";

pub(super) fn spawn(
    executable: Option<PathBuf>,
    commands: mpsc::Receiver<ProviderCommand>,
    events: mpsc::Sender<ProviderEvent>,
) {
    tokio::spawn(async move {
        let mut actor = Actor::new(
            executable.unwrap_or_else(|| PathBuf::from("claude")),
            events,
        );
        actor.run(commands).await;
    });
}

fn command_arguments(
    sandbox: super::SandboxAccess,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    let permission_mode = match sandbox {
        super::SandboxAccess::ReadOnly => "plan",
        super::SandboxAccess::WorkspaceWriteConfirmed => "manual",
    };
    let mut arguments = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--replay-user-messages",
        "--permission-mode",
        permission_mode,
        "--permission-prompt-tool",
        "stdio",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if sandbox == super::SandboxAccess::ReadOnly {
        arguments.extend(
            [
                "--safe-mode",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--tools",
                "Read,Glob,Grep,AskUserQuestion",
            ]
            .into_iter()
            .map(str::to_owned),
        );
    }
    if let Some(session_id) = resume_session_id {
        arguments.push("--resume".to_owned());
        arguments.push(session_id.to_owned());
    }
    arguments
}

struct Actor {
    executable: PathBuf,
    events: mpsc::Sender<ProviderEvent>,
    run_id: Option<String>,
    start: Option<StartOrResume>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    session_id: Option<String>,
    ready_emitted: bool,
    next_control_id: u64,
    next_turn_id: u64,
    pending_controls: BTreeMap<String, PendingControl>,
    pending_attentions: BTreeMap<String, NormalizedControlRequest>,
    current_turn: Option<CurrentTurn>,
    turn_deadline: Option<tokio::time::Instant>,
    quiesced: bool,
}

#[derive(Clone, Debug)]
struct CurrentTurn {
    id: String,
    working_emitted: bool,
    interrupt_requested: bool,
}

#[derive(Clone, Copy, Debug)]
enum PendingControlKind {
    Initialize,
    Interrupt,
}

#[derive(Clone, Copy, Debug)]
struct PendingControl {
    kind: PendingControlKind,
    deadline: tokio::time::Instant,
}

impl Actor {
    fn new(executable: PathBuf, events: mpsc::Sender<ProviderEvent>) -> Self {
        Self {
            executable,
            events,
            run_id: None,
            start: None,
            child: None,
            stdin: None,
            stdout: None,
            session_id: None,
            ready_emitted: false,
            next_control_id: 1,
            next_turn_id: 1,
            pending_controls: BTreeMap::new(),
            pending_attentions: BTreeMap::new(),
            current_turn: None,
            turn_deadline: None,
            quiesced: false,
        }
    }

    async fn run(&mut self, mut commands: mpsc::Receiver<ProviderCommand>) {
        while self.stdout.is_none() {
            let Some(command) = commands.recv().await else {
                return;
            };
            match command {
                ProviderCommand::StartOrResume(start) => {
                    self.run_id = Some(start.run_id.clone());
                    let cwd = start.cwd.clone();
                    let resume_session_id = start.resume_session_id.clone();
                    let sandbox = start.sandbox;
                    self.start = Some(start);
                    if self
                        .start_process(&cwd, sandbox, resume_session_id.as_deref())
                        .await
                        .is_err()
                    {
                        self.fail(TransportFailure::Spawn).await;
                        return;
                    }
                    if self
                        .send_control_request(
                            json!({"subtype": "initialize"}),
                            PendingControlKind::Initialize,
                        )
                        .await
                        .is_err()
                    {
                        self.fail(TransportFailure::Disconnected).await;
                        self.shutdown_child().await;
                        return;
                    }
                }
                ProviderCommand::Shutdown => return,
                _ => self.fail(TransportFailure::CommandRejected).await,
            }
        }

        loop {
            let deadline = self.next_deadline();
            let stdout = self.stdout.as_mut().expect("stdout is present after spawn");
            tokio::select! {
                () = sleep_until_deadline(deadline) => {
                    self.fail(TransportFailure::Timeout).await;
                    self.shutdown_child().await;
                    return;
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        self.shutdown_child().await;
                        return;
                    };
                    if self.handle_command(command).await {
                        return;
                    }
                }
                read = read_bounded_frame(stdout) => {
                    match read {
                        Ok(Some(frame)) => {
                            if self.handle_frame(&frame).await.is_err() {
                                self.fail(TransportFailure::Protocol).await;
                                self.shutdown_child().await;
                                return;
                            }
                        }
                        Ok(None) => {
                            self.fail(TransportFailure::Disconnected).await;
                            self.shutdown_child().await;
                            return;
                        }
                        Err(_) => {
                            self.fail(TransportFailure::Protocol).await;
                            self.shutdown_child().await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn start_process(
        &mut self,
        cwd: &Path,
        sandbox: super::SandboxAccess,
        resume_session_id: Option<&str>,
    ) -> Result<(), ()> {
        let mut child = Command::new(&self.executable)
            .args(command_arguments(sandbox, resume_session_id))
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| ())?;
        let stdin = child.stdin.take().ok_or(())?;
        let stdout = child.stdout.take().ok_or(())?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut buffer = [0_u8; STDERR_DRAIN_BUFFER_BYTES];
                loop {
                    match stderr.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                }
            });
        }
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout));
        Ok(())
    }

    async fn handle_command(&mut self, command: ProviderCommand) -> bool {
        match command {
            ProviderCommand::StartOrResume(_) => {
                self.fail(TransportFailure::CommandRejected).await;
            }
            ProviderCommand::SendTurn { input } => {
                if self.quiesced
                    || self.session_id.is_none()
                    || self.current_turn.is_some()
                    || !self.pending_attentions.is_empty()
                    || self.begin_turn(input).await.is_err()
                {
                    self.fail(TransportFailure::CommandRejected).await;
                }
            }
            ProviderCommand::Respond { token, response } => {
                if self.respond(token, response).await.is_err() {
                    self.fail(TransportFailure::CommandRejected).await;
                }
            }
            ProviderCommand::Interrupt => {
                let interrupt_is_pending = self
                    .pending_controls
                    .values()
                    .any(|pending| matches!(pending.kind, PendingControlKind::Interrupt));
                if self.current_turn.is_none() || interrupt_is_pending {
                    self.fail(TransportFailure::CommandRejected).await;
                } else if self
                    .send_control_request(
                        json!({"subtype": "interrupt"}),
                        PendingControlKind::Interrupt,
                    )
                    .await
                    .is_err()
                {
                    self.fail(TransportFailure::Disconnected).await;
                } else if let Some(turn) = self.current_turn.as_mut() {
                    turn.interrupt_requested = true;
                }
            }
            ProviderCommand::Quiesce => self.quiesced = true,
            ProviderCommand::Shutdown => {
                self.shutdown_child().await;
                self.emit(ProviderEvent::Stopped {
                    run_id: self.run_id(),
                })
                .await;
                return true;
            }
        }
        false
    }

    async fn handle_frame(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let value: Value = serde_json::from_slice(bytes).map_err(|_| ())?;
        let frame_type = value.get("type").and_then(Value::as_str).ok_or(())?;
        match frame_type {
            "system" => self.handle_system(&value).await,
            "assistant" => self.handle_assistant(&value).await,
            "user" => self.handle_user_replay(&value).await,
            "result" => self.handle_result(&value).await,
            "control_request" => self.handle_control_request(&value).await,
            "control_response" => self.handle_control_response(&value).await,
            "control_cancel_request" => self.handle_control_cancel(&value),
            _ => Ok(()),
        }
    }

    async fn handle_system(&mut self, value: &Value) -> Result<(), ()> {
        if value.get("subtype").and_then(Value::as_str) != Some("init") {
            self.refresh_turn_deadline();
            return Ok(());
        }
        if self
            .start
            .as_ref()
            .is_some_and(|start| start.sandbox == super::SandboxAccess::ReadOnly)
        {
            validate_read_only_init(value)?;
        }
        let session_id = required_identifier(value, "/session_id")?;
        if let Some(expected) = self
            .start
            .as_ref()
            .and_then(|start| start.resume_session_id.as_deref())
        {
            if expected != session_id {
                return Err(());
            }
        }
        if self
            .session_id
            .as_ref()
            .is_some_and(|existing| existing != &session_id)
        {
            return Err(());
        }
        self.session_id = Some(session_id.clone());
        self.emit_ready_if_needed(session_id).await;
        self.emit_working_if_needed().await;
        self.refresh_turn_deadline();
        Ok(())
    }

    async fn handle_assistant(&mut self, value: &Value) -> Result<(), ()> {
        self.require_current_session(value)?;
        let turn_id = self.current_turn.as_ref().ok_or(())?.id.clone();
        let content = value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .ok_or(())?;
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let text = block.get("text").and_then(Value::as_str).ok_or(())?;
            let text = sanitized_visible(text, MAX_VISIBLE_TEXT_BYTES);
            if !text.is_empty() {
                self.emit(ProviderEvent::OutputDelta {
                    run_id: self.run_id(),
                    turn_id: turn_id.clone(),
                    text,
                })
                .await;
            }
        }
        self.refresh_turn_deadline();
        Ok(())
    }

    async fn handle_user_replay(&mut self, value: &Value) -> Result<(), ()> {
        self.require_current_session(value)?;
        if value.get("isReplay").and_then(Value::as_bool) != Some(true) {
            return Err(());
        }
        self.emit_working_if_needed().await;
        self.refresh_turn_deadline();
        Ok(())
    }

    async fn handle_result(&mut self, value: &Value) -> Result<(), ()> {
        self.require_current_session(value)?;
        if !self.pending_attentions.is_empty() {
            return Err(());
        }
        let subtype = value.get("subtype").and_then(Value::as_str).ok_or(())?;
        let is_error = value.get("is_error").and_then(Value::as_bool).ok_or(())?;
        let provider_outcome = match (subtype, is_error) {
            ("success", false) => TurnOutcome::Completed,
            (
                "success"
                | "error_during_execution"
                | "error_max_turns"
                | "error_max_budget_usd"
                | "error_max_structured_output_retries",
                true,
            ) => TurnOutcome::Failed,
            _ => return Err(()),
        };
        let turn = self.current_turn.take().ok_or(())?;
        let outcome = if turn.interrupt_requested {
            TurnOutcome::Interrupted
        } else {
            provider_outcome
        };
        self.turn_deadline = None;
        self.emit(ProviderEvent::TurnCompleted {
            run_id: self.run_id(),
            turn_id: turn.id,
            outcome,
        })
        .await;
        Ok(())
    }

    async fn handle_control_request(&mut self, value: &Value) -> Result<(), ()> {
        let request = parse_control_request(value)?;
        if self
            .start
            .as_ref()
            .is_some_and(|start| start.sandbox == super::SandboxAccess::ReadOnly)
            && !request.is_question()
        {
            return Err(());
        }
        let session_id = self.session_id.clone().ok_or(())?;
        let turn_id = self.current_turn.as_ref().ok_or(())?.id.clone();
        let audit_id = RpcId::String(request.request_id.clone()).audit_id();
        if self.pending_attentions.contains_key(&audit_id) {
            return Err(());
        }
        let method = request.method().to_owned();
        let attention = ProviderAttention {
            token: ResponseToken {
                rpc_id: RpcId::String(request.request_id.clone()),
                method,
                request_id: audit_id.clone(),
            },
            class: request.class(),
            thread_id: session_id,
            turn_id,
            item_id: request.tool_use_id.clone(),
            requested_action: request.requested_action.clone(),
            questions: request.provider_questions(),
        };
        self.pending_attentions.insert(audit_id, request);
        self.turn_deadline = None;
        self.emit(ProviderEvent::AttentionRequested {
            run_id: self.run_id(),
            attention,
        })
        .await;
        Ok(())
    }

    async fn handle_control_response(&mut self, value: &Value) -> Result<(), ()> {
        let response = value.get("response").ok_or(())?;
        let request_id = required_identifier(response, "/request_id")?;
        let pending = self.pending_controls.remove(&request_id).ok_or(())?;
        if response.get("subtype").and_then(Value::as_str) != Some("success") {
            return Err(());
        }
        match pending.kind {
            PendingControlKind::Initialize => {
                if let Some(session_id) = self
                    .start
                    .as_ref()
                    .and_then(|start| start.resume_session_id.clone())
                {
                    self.session_id = Some(session_id.clone());
                    self.emit_ready_if_needed(session_id).await;
                }
                let initial_input = self
                    .start
                    .as_ref()
                    .map(|start| start.initial_input.clone())
                    .unwrap_or_default();
                if initial_input.trim().is_empty() {
                    self.turn_deadline = None;
                } else {
                    self.begin_turn(initial_input).await?;
                }
            }
            PendingControlKind::Interrupt => self.refresh_turn_deadline(),
        }
        Ok(())
    }

    fn handle_control_cancel(&mut self, value: &Value) -> Result<(), ()> {
        let request_id = required_identifier(value, "/request_id")?;
        let audit_id = RpcId::String(request_id).audit_id();
        self.pending_attentions.remove(&audit_id).ok_or(())?;
        self.refresh_turn_deadline();
        Ok(())
    }

    async fn begin_turn(&mut self, input: String) -> Result<(), ()> {
        if self.current_turn.is_some() || input.trim().is_empty() {
            return Err(());
        }
        let turn_number = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.checked_add(1).ok_or(())?;
        self.send_raw(json!({
            "type": "user",
            "message": {"role": "user", "content": input},
            "parent_tool_use_id": Value::Null,
        }))
        .await?;
        self.current_turn = Some(CurrentTurn {
            id: format!("turn-{turn_number}"),
            working_emitted: false,
            interrupt_requested: false,
        });
        self.refresh_turn_deadline();
        self.emit_working_if_needed().await;
        Ok(())
    }

    async fn respond(
        &mut self,
        token: ResponseToken,
        response: ProviderResponse,
    ) -> Result<(), ()> {
        let request = self
            .pending_attentions
            .get(&token.request_id)
            .cloned()
            .ok_or(())?;
        if request.method() != token.method
            || token.rpc_id != RpcId::String(request.request_id.clone())
        {
            return Err(());
        }
        let response_payload = request.response_payload(response)?;
        self.send_raw(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request.request_id,
                "response": response_payload,
            }
        }))
        .await?;
        self.pending_attentions.remove(&token.request_id);
        if self.pending_attentions.is_empty() {
            self.refresh_turn_deadline();
        }
        self.emit(ProviderEvent::ResponseResolved {
            run_id: self.run_id(),
            request_id: token.request_id,
        })
        .await;
        Ok(())
    }

    async fn send_control_request(
        &mut self,
        request: Value,
        kind: PendingControlKind,
    ) -> Result<(), ()> {
        let sequence = self.next_control_id;
        self.next_control_id = self.next_control_id.checked_add(1).ok_or(())?;
        let request_id = format!("managed-{sequence}");
        self.send_raw(json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        }))
        .await?;
        self.pending_controls.insert(
            request_id,
            PendingControl {
                kind,
                deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT,
            },
        );
        Ok(())
    }

    async fn send_raw(&mut self, value: Value) -> Result<(), ()> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| ())?;
        if bytes.len() > MAX_PROVIDER_FRAME_BYTES {
            return Err(());
        }
        bytes.push(b'\n');
        let stdin = self.stdin.as_mut().ok_or(())?;
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            stdin.write_all(&bytes).await.map_err(|_| ())?;
            stdin.flush().await.map_err(|_| ())
        })
        .await
        .map_err(|_| ())?
    }

    fn require_current_session(&self, value: &Value) -> Result<(), ()> {
        let session_id = required_identifier(value, "/session_id")?;
        if self.session_id.as_deref() == Some(session_id.as_str()) {
            Ok(())
        } else {
            Err(())
        }
    }

    async fn emit_ready_if_needed(&mut self, session_id: String) {
        if self.ready_emitted {
            return;
        }
        self.ready_emitted = true;
        self.emit(ProviderEvent::Ready {
            run_id: self.run_id(),
            session_id,
        })
        .await;
    }

    async fn emit_working_if_needed(&mut self) {
        if self.session_id.is_none() {
            return;
        }
        let turn_id = match self.current_turn.as_mut() {
            Some(turn) if !turn.working_emitted => {
                turn.working_emitted = true;
                turn.id.clone()
            }
            _ => return,
        };
        self.emit(ProviderEvent::Working {
            run_id: self.run_id(),
            turn_id,
        })
        .await;
    }

    fn refresh_turn_deadline(&mut self) {
        self.turn_deadline = if self.current_turn.is_some() && self.pending_attentions.is_empty() {
            Some(tokio::time::Instant::now() + TURN_IDLE_TIMEOUT)
        } else {
            None
        };
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.pending_controls
            .values()
            .map(|pending| pending.deadline)
            .chain(self.turn_deadline)
            .min()
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

    fn run_id(&self) -> String {
        self.run_id
            .clone()
            .unwrap_or_else(|| "unassigned".to_owned())
    }

    async fn shutdown_child(&mut self) {
        self.stdin.take();
        self.stdout.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

#[derive(Clone, Debug)]
struct NormalizedControlRequest {
    request_id: String,
    tool_use_id: String,
    requested_action: String,
    kind: NormalizedRequestKind,
}

#[derive(Clone, Debug)]
enum NormalizedRequestKind {
    Permission {
        class: AttentionClass,
        session_suggestions: Vec<Value>,
    },
    Questions {
        questions: Vec<Question>,
    },
}

impl NormalizedControlRequest {
    fn class(&self) -> AttentionClass {
        match &self.kind {
            NormalizedRequestKind::Permission { class, .. } => class.clone(),
            NormalizedRequestKind::Questions { .. } => AttentionClass::UserInput,
        }
    }

    #[cfg(test)]
    fn requested_action(&self) -> &str {
        &self.requested_action
    }

    fn is_question(&self) -> bool {
        matches!(self.kind, NormalizedRequestKind::Questions { .. })
    }

    fn method(&self) -> &'static str {
        match self.kind {
            NormalizedRequestKind::Permission { .. } => PERMISSION_METHOD,
            NormalizedRequestKind::Questions { .. } => QUESTION_METHOD,
        }
    }

    fn provider_questions(&self) -> Vec<ProviderQuestion> {
        let NormalizedRequestKind::Questions { questions } = &self.kind else {
            return Vec::new();
        };
        questions
            .iter()
            .map(|question| ProviderQuestion {
                id: question.question.clone(),
                header: question.header.clone(),
                prompt: question.question.clone(),
                options: question
                    .options
                    .iter()
                    .map(|option| ProviderQuestionOption {
                        label: option.label.clone(),
                        description: option.description.clone(),
                    })
                    .collect(),
                multiple: question.multi_select,
                custom_allowed: true,
            })
            .collect()
    }

    #[cfg(test)]
    fn session_suggestions(&self) -> &[Value] {
        match &self.kind {
            NormalizedRequestKind::Permission {
                session_suggestions,
                ..
            } => session_suggestions,
            NormalizedRequestKind::Questions { .. } => &[],
        }
    }

    fn response_payload(&self, response: ProviderResponse) -> Result<Value, ()> {
        match (&self.kind, response) {
            (NormalizedRequestKind::Permission { .. }, ProviderResponse::Approve) => Ok(json!({
                "behavior": "allow",
                "toolUseID": self.tool_use_id,
                "decisionClassification": "user_temporary",
            })),
            (
                NormalizedRequestKind::Permission {
                    session_suggestions,
                    ..
                },
                ProviderResponse::ApproveForSession,
            ) if !session_suggestions.is_empty() => Ok(json!({
                "behavior": "allow",
                "updatedPermissions": session_suggestions,
                "toolUseID": self.tool_use_id,
                "decisionClassification": "user_permanent",
            })),
            (NormalizedRequestKind::Permission { .. }, ProviderResponse::Decline) => Ok(json!({
                "behavior": "deny",
                "message": "User declined this action",
                "toolUseID": self.tool_use_id,
                "decisionClassification": "user_reject",
            })),
            (
                NormalizedRequestKind::Questions { questions },
                ProviderResponse::Answers(answers),
            ) => question_response(&self.tool_use_id, questions, answers),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug)]
struct Question {
    question: String,
    header: String,
    options: Vec<QuestionOption>,
    multi_select: bool,
}

#[derive(Clone, Debug)]
struct QuestionOption {
    label: String,
    description: String,
    preview: Option<String>,
}

impl Question {
    fn to_json(&self) -> Value {
        let options = self
            .options
            .iter()
            .map(|option| {
                let mut value = json!({
                    "label": option.label,
                    "description": option.description,
                });
                if let Some(preview) = &option.preview {
                    value["preview"] = Value::String(preview.clone());
                }
                value
            })
            .collect::<Vec<_>>();
        json!({
            "question": self.question,
            "header": self.header,
            "options": options,
            "multiSelect": self.multi_select,
        })
    }
}

fn parse_control_request(value: &Value) -> Result<NormalizedControlRequest, ()> {
    if value.get("type").and_then(Value::as_str) != Some("control_request") {
        return Err(());
    }
    let request_id = required_identifier(value, "/request_id")?;
    let request = value.get("request").ok_or(())?;
    if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return Err(());
    }
    let tool_name = required_identifier(request, "/tool_name")?;
    let tool_use_id = required_identifier(request, "/tool_use_id")?;
    let input = request.get("input").and_then(Value::as_object).ok_or(())?;
    if tool_name == "AskUserQuestion" {
        let questions = parse_questions(input.get("questions").ok_or(())?)?;
        let requested_action = questions.first().ok_or(())?.question.clone();
        return Ok(NormalizedControlRequest {
            request_id,
            tool_use_id,
            requested_action,
            kind: NormalizedRequestKind::Questions { questions },
        });
    }

    let class = match tool_name.as_str() {
        "Bash" => AttentionClass::CommandApproval,
        "Edit" | "Write" | "NotebookEdit" => AttentionClass::FileChangeApproval,
        _ => AttentionClass::PermissionApproval,
    };
    let requested_action = permission_requested_action(request, input, &tool_name);
    let session_suggestions = normalize_session_suggestions(
        request
            .get("permission_suggestions")
            .unwrap_or(&Value::Null),
    );
    Ok(NormalizedControlRequest {
        request_id,
        tool_use_id,
        requested_action,
        kind: NormalizedRequestKind::Permission {
            class,
            session_suggestions,
        },
    })
}

fn parse_questions(value: &Value) -> Result<Vec<Question>, ()> {
    let values = value.as_array().ok_or(())?;
    if !(1..=4).contains(&values.len()) {
        return Err(());
    }
    let mut seen_questions = BTreeSet::new();
    values
        .iter()
        .map(|value| {
            let question = required_visible(value, "/question", MAX_QUESTION_BYTES)?;
            if !seen_questions.insert(question.clone()) {
                return Err(());
            }
            let header = required_visible(value, "/header", 128)?;
            let option_values = value.get("options").and_then(Value::as_array).ok_or(())?;
            if !(2..=4).contains(&option_values.len()) {
                return Err(());
            }
            let mut seen_labels = BTreeSet::new();
            let options = option_values
                .iter()
                .map(|option| {
                    let label = required_visible(option, "/label", 256)?;
                    if !seen_labels.insert(label.clone()) {
                        return Err(());
                    }
                    let description = required_visible(option, "/description", 2 * 1024)?;
                    let preview = option
                        .get("preview")
                        .and_then(Value::as_str)
                        .map(|preview| sanitized_visible(preview, MAX_VISIBLE_TEXT_BYTES));
                    Ok(QuestionOption {
                        label,
                        description,
                        preview,
                    })
                })
                .collect::<Result<Vec<_>, ()>>()?;
            let multi_select = value
                .get("multiSelect")
                .and_then(Value::as_bool)
                .ok_or(())?;
            Ok(Question {
                question,
                header,
                options,
                multi_select,
            })
        })
        .collect()
}

fn question_response(
    tool_use_id: &str,
    questions: &[Question],
    answers: BTreeMap<String, Vec<String>>,
) -> Result<Value, ()> {
    if answers.len() != questions.len() {
        return Err(());
    }
    let mut normalized_answers = Map::new();
    for question in questions {
        let selected = answers.get(&question.question).ok_or(())?;
        if selected.is_empty() || (!question.multi_select && selected.len() != 1) {
            return Err(());
        }
        let selected = selected
            .iter()
            .map(|answer| {
                let answer = sanitized_visible(answer, MAX_ANSWER_BYTES);
                if answer.is_empty() {
                    Err(())
                } else {
                    Ok(answer)
                }
            })
            .collect::<Result<Vec<_>, ()>>()?;
        normalized_answers.insert(
            question.question.clone(),
            Value::String(selected.join(", ")),
        );
    }
    Ok(json!({
        "behavior": "allow",
        "updatedInput": {
            "questions": questions.iter().map(Question::to_json).collect::<Vec<_>>(),
            "answers": normalized_answers,
        },
        "toolUseID": tool_use_id,
        "decisionClassification": "user_temporary",
    }))
}

fn permission_requested_action(
    request: &Value,
    input: &Map<String, Value>,
    tool_name: &str,
) -> String {
    let raw = request
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| request.get("description").and_then(Value::as_str))
        .or_else(|| request.get("decision_reason").and_then(Value::as_str))
        .or_else(|| input.get("command").and_then(Value::as_str))
        .or_else(|| input.get("file_path").and_then(Value::as_str))
        .unwrap_or(tool_name);
    sanitized_visible(raw, MAX_VISIBLE_TEXT_BYTES)
}

fn normalize_session_suggestions(value: &Value) -> Vec<Value> {
    let Some(suggestions) = value.as_array() else {
        return Vec::new();
    };
    suggestions
        .iter()
        .filter_map(|suggestion| {
            if suggestion.get("type").and_then(Value::as_str) != Some("addRules")
                || suggestion.get("behavior").and_then(Value::as_str) != Some("allow")
                || suggestion.get("destination").and_then(Value::as_str) != Some("session")
            {
                return None;
            }
            let rules = suggestion.get("rules")?.as_array()?;
            if rules.is_empty() || rules.len() > 64 {
                return None;
            }
            let rules = rules
                .iter()
                .map(|rule| {
                    let tool_name = required_identifier(rule, "/toolName").ok()?;
                    let mut normalized = json!({"toolName": tool_name});
                    if let Some(content) = rule.get("ruleContent").and_then(Value::as_str) {
                        let content = sanitized_visible(content, MAX_VISIBLE_TEXT_BYTES);
                        if content.is_empty() {
                            return None;
                        }
                        normalized["ruleContent"] = Value::String(content);
                    }
                    Some(normalized)
                })
                .collect::<Option<Vec<_>>>()?;
            Some(json!({
                "type": "addRules",
                "rules": rules,
                "behavior": "allow",
                "destination": "session",
            }))
        })
        .collect()
}

fn validate_read_only_init(value: &Value) -> Result<(), ()> {
    const EXPECTED_TOOLS: [&str; 4] = ["AskUserQuestion", "Glob", "Grep", "Read"];

    if value.get("permissionMode").and_then(Value::as_str) != Some("plan")
        || !value
            .get("mcp_servers")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        return Err(());
    }
    let tools = value.get("tools").and_then(Value::as_array).ok_or(())?;
    let tools = tools
        .iter()
        .map(Value::as_str)
        .collect::<Option<BTreeSet<_>>>()
        .ok_or(())?;
    if tools == EXPECTED_TOOLS.into_iter().collect() {
        Ok(())
    } else {
        Err(())
    }
}

async fn read_bounded_frame(reader: &mut BufReader<ChildStdout>) -> Result<Option<Vec<u8>>, ()> {
    use tokio::io::AsyncBufReadExt as _;

    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|_| ())?;
        if available.is_empty() {
            return if frame.is_empty() { Ok(None) } else { Err(()) };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(take) > MAX_PROVIDER_FRAME_BYTES {
            return Err(());
        }
        frame.extend_from_slice(&available[..take]);
        let terminated = available[take - 1] == b'\n';
        reader.consume(take);
        if terminated {
            return Ok(Some(frame));
        }
    }
}

async fn sleep_until_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn required_identifier(value: &Value, pointer: &str) -> Result<String, ()> {
    let value = value.pointer(pointer).and_then(Value::as_str).ok_or(())?;
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control)
    {
        return Err(());
    }
    Ok(value.to_owned())
}

fn required_visible(value: &Value, pointer: &str, max_bytes: usize) -> Result<String, ()> {
    let raw = value.pointer(pointer).and_then(Value::as_str).ok_or(())?;
    let value = sanitized_visible(raw, max_bytes);
    if value.is_empty() {
        Err(())
    } else {
        Ok(value)
    }
}

fn sanitized_visible(value: &str, max_bytes: usize) -> String {
    let sanitized = value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    if sanitized.len() <= max_bytes {
        return sanitized;
    }
    let mut end = max_bytes;
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;
    use crate::managed_provider::{
        AttentionClass, ProviderCommand, ProviderEvent, SandboxAccess, StartOrResume,
        TransportFailure, TurnOutcome,
    };

    #[test]
    fn cli_arguments_match_the_bidirectional_stream_contract_by_sandbox() {
        assert_eq!(
            command_arguments(SandboxAccess::WorkspaceWriteConfirmed, None),
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--replay-user-messages",
                "--permission-mode",
                "manual",
                "--permission-prompt-tool",
                "stdio",
            ]
        );
        assert_eq!(
            command_arguments(SandboxAccess::WorkspaceWriteConfirmed, Some("session-1")),
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--replay-user-messages",
                "--permission-mode",
                "manual",
                "--permission-prompt-tool",
                "stdio",
                "--resume",
                "session-1",
            ]
        );
        assert_eq!(
            command_arguments(SandboxAccess::ReadOnly, None),
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--replay-user-messages",
                "--permission-mode",
                "plan",
                "--permission-prompt-tool",
                "stdio",
                "--safe-mode",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--tools",
                "Read,Glob,Grep,AskUserQuestion",
            ]
        );
    }

    #[test]
    fn permission_and_question_requests_are_normalized_without_raw_payloads() {
        let permission = parse_control_request(&json!({
            "type": "control_request",
            "request_id": "permission-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "tool_use_id": "tool-1",
                "input": {"command": "cargo test", "secret": "must-not-escape"},
                "title": "Run cargo test?",
                "permission_suggestions": [{
                    "type": "addRules",
                    "rules": [{"toolName": "Bash", "ruleContent": "cargo test"}],
                    "behavior": "allow",
                    "destination": "session"
                }, {
                    "type": "addRules",
                    "rules": [{"toolName": "Bash", "ruleContent": "cargo test"}],
                    "behavior": "allow",
                    "destination": "localSettings"
                }]
            }
        }))
        .unwrap();
        assert_eq!(permission.class(), AttentionClass::CommandApproval);
        assert_eq!(permission.requested_action(), "Run cargo test?");
        assert!(!format!("{permission:?}").contains("must-not-escape"));
        assert_eq!(permission.session_suggestions().len(), 1);
        let permission_response = permission
            .response_payload(ProviderResponse::ApproveForSession)
            .unwrap();
        assert_eq!(
            permission_response
                .get("updatedPermissions")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        let question = parse_control_request(&json!({
            "type": "control_request",
            "request_id": "question-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_use_id": "tool-2",
                "input": {"questions": [{
                    "question": "Which database?",
                    "header": "Database",
                    "options": [
                        {"label": "Postgres", "description": "Relational"},
                        {"label": "SQLite", "description": "Embedded"}
                    ],
                    "multiSelect": false
                }]}
            }
        }))
        .unwrap();
        assert_eq!(question.class(), AttentionClass::UserInput);
        assert_eq!(question.requested_action(), "Which database?");
        let provider_questions = question.provider_questions();
        assert_eq!(provider_questions[0].id, "Which database?");
        assert_eq!(provider_questions[0].options[0].label, "Postgres");
        let question_response = question
            .response_payload(ProviderResponse::Answers(BTreeMap::from([(
                "Which database?".to_owned(),
                vec!["Postgres".to_owned()],
            )])))
            .unwrap();
        assert_eq!(
            question_response.pointer("/updatedInput/answers/Which database?"),
            Some(&Value::String("Postgres".to_owned()))
        );
    }

    #[test]
    fn read_only_init_requires_the_exact_hermetic_tool_surface() {
        let init = json!({
            "permissionMode": "plan",
            "mcp_servers": [],
            "tools": ["AskUserQuestion", "Glob", "Grep", "Read"],
        });
        assert_eq!(validate_read_only_init(&init), Ok(()));

        let mut unsafe_init = init.clone();
        unsafe_init["tools"] = json!(["AskUserQuestion", "Bash", "Glob", "Grep", "Read"]);
        assert_eq!(validate_read_only_init(&unsafe_init), Err(()));

        unsafe_init = init;
        unsafe_init["mcp_servers"] = json!([{"name": "filesystem"}]);
        assert_eq!(validate_read_only_init(&unsafe_init), Err(()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actor_drains_stderr_and_requires_a_valid_terminal_result_before_success() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-claude");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
IFS= read -r initialize
i=0
while [ "$i" -lt 2048 ]; do
    printf '%s\n' '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' >&2
    i=$((i + 1))
done
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"managed-1","response":{}}}'
IFS= read -r prompt
printf '%s\n' '{"type":"system","subtype":"init","session_id":"session-live"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]},"session_id":"session-live"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"session_id":"session-live"}'
while IFS= read -r line; do :; done
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let (command_tx, command_rx) = mpsc::channel(4);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        spawn(Some(executable), command_rx, event_tx);
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-live".into(),
                cwd: directory.path().to_path_buf(),
                resume_session_id: None,
                initial_input: "Do the work".into(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .await
            .unwrap();

        assert_eq!(
            event_rx.recv().await,
            Some(ProviderEvent::Ready {
                run_id: "run-live".into(),
                session_id: "session-live".into(),
            })
        );
        assert_eq!(
            event_rx.recv().await,
            Some(ProviderEvent::Working {
                run_id: "run-live".into(),
                turn_id: "turn-1".into(),
            })
        );
        assert_eq!(
            event_rx.recv().await,
            Some(ProviderEvent::OutputDelta {
                run_id: "run-live".into(),
                turn_id: "turn-1".into(),
                text: "done".into(),
            })
        );
        assert_eq!(
            event_rx.recv().await,
            Some(ProviderEvent::TurnCompleted {
                run_id: "run-live".into(),
                turn_id: "turn-1".into(),
                outcome: TurnOutcome::Completed,
            })
        );
        command_tx.send(ProviderCommand::Shutdown).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actor_reports_disconnect_instead_of_success_when_result_is_missing() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("incomplete-claude");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"managed-1","response":{}}}'
IFS= read -r prompt
printf '%s\n' '{"type":"system","subtype":"init","session_id":"session-incomplete"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]},"session_id":"session-incomplete"}'
exit 0
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let (command_tx, command_rx) = mpsc::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        spawn(Some(executable), command_rx, event_tx);
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-incomplete".into(),
                cwd: directory.path().to_path_buf(),
                resume_session_id: None,
                initial_input: "work".into(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await
        {
            let terminal = matches!(event, ProviderEvent::TransportFailed { .. });
            events.push(event);
            if terminal {
                break;
            }
        }
        assert!(events.contains(&ProviderEvent::TransportFailed {
            run_id: "run-incomplete".into(),
            reason: TransportFailure::Disconnected,
        }));
        assert!(!events.iter().any(|event| matches!(
            event,
            ProviderEvent::TurnCompleted {
                outcome: TurnOutcome::Completed,
                ..
            }
        )));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actor_rejects_an_oversized_provider_frame() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("oversized-claude");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
IFS= read -r initialize
dd if=/dev/zero bs=1048577 count=1 2>/dev/null | tr '\000' a
printf '\n'
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let (command_tx, command_rx) = mpsc::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(2);
        spawn(Some(executable), command_rx, event_tx);
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-oversized".into(),
                cwd: directory.path().to_path_buf(),
                resume_session_id: None,
                initial_input: "work".into(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .await
            .unwrap();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                .await
                .unwrap(),
            Some(ProviderEvent::TransportFailed {
                run_id: "run-oversized".into(),
                reason: TransportFailure::Protocol,
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actor_times_out_a_provider_that_never_acknowledges_initialize() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("stalled-claude");
        std::fs::write(
            &executable,
            "#!/bin/sh\nwhile IFS= read -r line; do :; done\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let (command_tx, command_rx) = mpsc::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(2);
        spawn(Some(executable), command_rx, event_tx);
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-stalled".into(),
                cwd: PathBuf::from(directory.path()),
                resume_session_id: None,
                initial_input: "work".into(),
                sandbox: SandboxAccess::WorkspaceWriteConfirmed,
            }))
            .await
            .unwrap();

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
                .await
                .unwrap(),
            Some(ProviderEvent::TransportFailed {
                run_id: "run-stalled".into(),
                reason: TransportFailure::Timeout,
            })
        );
    }
}
