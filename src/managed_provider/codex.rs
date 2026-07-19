use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Stdio,
    time::{Duration, SystemTime},
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncWriteExt as _, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::mpsc,
};

use super::{
    AttentionClass, ProviderAttention, ProviderCommand, ProviderEvent, ProviderQuestion,
    ProviderQuestionOption, ProviderResponse, RpcId, StartOrResume, TransportFailure, TurnOutcome,
};

const MAX_PROVIDER_FRAME_BYTES: usize = 1024 * 1024;
const MAX_VISIBLE_TEXT_BYTES: usize = 16 * 1024;
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn spawn(
    executable: Option<PathBuf>,
    commands: mpsc::Receiver<ProviderCommand>,
    events: mpsc::Sender<ProviderEvent>,
) {
    tokio::spawn(async move {
        let mut actor = Actor::new(executable.unwrap_or_else(|| PathBuf::from("codex")), events);
        actor.run(commands).await;
    });
}

struct Actor {
    executable: PathBuf,
    events: mpsc::Sender<ProviderEvent>,
    run_id: Option<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    next_request_id: u64,
    pending: BTreeMap<u64, PendingCall>,
    start: Option<StartOrResume>,
    session_id: Option<String>,
    current_turn_id: Option<String>,
    quiesced: bool,
}

#[derive(Debug)]
enum PendingRequest {
    Initialize,
    ThreadStart,
    ThreadResume,
    TurnStart,
    Interrupt,
}

#[derive(Debug)]
struct PendingCall {
    request: PendingRequest,
    deadline: tokio::time::Instant,
}

impl Actor {
    fn new(executable: PathBuf, events: mpsc::Sender<ProviderEvent>) -> Self {
        Self {
            executable,
            events,
            run_id: None,
            child: None,
            stdin: None,
            stdout: None,
            next_request_id: 1,
            pending: BTreeMap::new(),
            start: None,
            session_id: None,
            current_turn_id: None,
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
                    self.start = Some(start);
                    if self.start_process().await.is_err() {
                        self.fail(TransportFailure::Spawn).await;
                        return;
                    }
                    if self.send_initialize().await.is_err() {
                        self.fail(TransportFailure::Disconnected).await;
                        return;
                    }
                }
                ProviderCommand::Shutdown => return,
                _ => self.fail(TransportFailure::CommandRejected).await,
            }
        }

        loop {
            let request_deadline = self.pending.values().map(|call| call.deadline).min();
            let stdout = self.stdout.as_mut().expect("stdout is present after spawn");
            tokio::select! {
                () = sleep_until_request_deadline(request_deadline) => {
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
                        Ok(None) | Err(_) => {
                            self.fail(TransportFailure::Disconnected).await;
                            self.shutdown_child().await;
                            return;
                        }
                        Ok(Some(read_frame)) => {
                            if self.handle_frame(&read_frame).await.is_err() {
                                self.fail(TransportFailure::Protocol).await;
                                self.shutdown_child().await;
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn start_process(&mut self) -> Result<(), ()> {
        let mut child = Command::new(&self.executable)
            .args(["app-server", "--listen", "stdio://"])
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
                let mut buffer = [0_u8; 4096];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut stderr, &mut buffer).await {
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

    async fn send_initialize(&mut self) -> Result<(), ()> {
        self.send_request(
            "initialize",
            initialize_params(),
            PendingRequest::Initialize,
        )
        .await
    }

    async fn handle_command(&mut self, command: ProviderCommand) -> bool {
        match command {
            ProviderCommand::StartOrResume(_) => {
                self.fail(TransportFailure::CommandRejected).await;
            }
            ProviderCommand::SendTurn { input } => {
                if self.quiesced || self.send_turn(input).await.is_err() {
                    self.fail(TransportFailure::CommandRejected).await;
                }
            }
            ProviderCommand::Respond { token, response } => {
                if self.send_provider_response(token, response).await.is_err() {
                    self.fail(TransportFailure::CommandRejected).await;
                }
            }
            ProviderCommand::Interrupt => {
                let Some(thread_id) = self.session_id.clone() else {
                    self.fail(TransportFailure::CommandRejected).await;
                    return false;
                };
                let Some(turn_id) = self.current_turn_id.clone() else {
                    self.fail(TransportFailure::CommandRejected).await;
                    return false;
                };
                if self
                    .send_request(
                        "turn/interrupt",
                        json!({"threadId": thread_id, "turnId": turn_id}),
                        PendingRequest::Interrupt,
                    )
                    .await
                    .is_err()
                {
                    self.fail(TransportFailure::Disconnected).await;
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
        if value.get("method").is_some() && value.get("id").is_some() {
            return self.handle_server_request(value).await;
        }
        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            return self.handle_response(id, &value).await;
        }
        if value.get("method").is_some() {
            return self.handle_notification(&value).await;
        }
        Err(())
    }

    async fn handle_response(&mut self, id: u64, value: &Value) -> Result<(), ()> {
        let pending = self.pending.remove(&id).ok_or(())?.request;
        if value.get("error").is_some() {
            return Err(());
        }
        let result = value.get("result").ok_or(())?;
        match pending {
            PendingRequest::Initialize => {
                self.send_raw(json!({"method": "initialized"})).await?;
                self.send_thread_start_or_resume().await?;
            }
            PendingRequest::ThreadStart | PendingRequest::ThreadResume => {
                let session_id = result
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .ok_or(())?
                    .to_owned();
                self.session_id = Some(session_id.clone());
                self.emit(ProviderEvent::Ready {
                    run_id: self.run_id(),
                    session_id,
                })
                .await;
                let initial_input = self
                    .start
                    .as_ref()
                    .map(|start| start.initial_input.clone())
                    .unwrap_or_default();
                if !initial_input.trim().is_empty() {
                    self.send_turn(initial_input).await?;
                }
            }
            PendingRequest::TurnStart => {
                let turn_id = result
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .ok_or(())?
                    .to_owned();
                self.current_turn_id = Some(turn_id.clone());
                self.emit(ProviderEvent::Working {
                    run_id: self.run_id(),
                    turn_id,
                })
                .await;
            }
            PendingRequest::Interrupt => {}
        }
        Ok(())
    }

    async fn send_thread_start_or_resume(&mut self) -> Result<(), ()> {
        let start = self.start.as_ref().ok_or(())?;
        match &start.resume_session_id {
            Some(session_id) => {
                self.send_request(
                    "thread/resume",
                    thread_resume_params(session_id),
                    PendingRequest::ThreadResume,
                )
                .await
            }
            None => {
                self.send_request(
                    "thread/start",
                    thread_start_params(&start.cwd, start.sandbox),
                    PendingRequest::ThreadStart,
                )
                .await
            }
        }
    }

    async fn send_turn(&mut self, input: String) -> Result<(), ()> {
        let thread_id = self.session_id.clone().ok_or(())?;
        self.send_request(
            "turn/start",
            turn_start_params(&thread_id, input),
            PendingRequest::TurnStart,
        )
        .await
    }

    async fn handle_notification(&mut self, value: &Value) -> Result<(), ()> {
        let method = value.get("method").and_then(Value::as_str).ok_or(())?;
        let params = value.get("params").unwrap_or(&Value::Null);
        match method {
            "turn/started" => {
                let turn_id = required_string(params, "/turn/id")?;
                self.current_turn_id = Some(turn_id.clone());
                self.emit(ProviderEvent::Working {
                    run_id: self.run_id(),
                    turn_id,
                })
                .await;
            }
            "item/agentMessage/delta" => {
                let turn_id = required_string(params, "/turnId")?;
                let text = required_string(params, "/delta")?;
                self.emit(ProviderEvent::OutputDelta {
                    run_id: self.run_id(),
                    turn_id,
                    text: bounded_text(&text),
                })
                .await;
            }
            "turn/completed" => {
                let turn_id = required_string(params, "/turn/id")?;
                let outcome = match params.pointer("/turn/status").and_then(Value::as_str) {
                    Some("completed") => TurnOutcome::Completed,
                    Some("interrupted") => TurnOutcome::Interrupted,
                    Some("failed") => TurnOutcome::Failed,
                    _ => return Err(()),
                };
                self.current_turn_id = None;
                self.emit(ProviderEvent::TurnCompleted {
                    run_id: self.run_id(),
                    turn_id,
                    outcome,
                })
                .await;
            }
            "serverRequest/resolved" => {
                let request_id = params
                    .get("requestId")
                    .and_then(RpcId::from_json)
                    .ok_or(())?
                    .audit_id();
                self.emit(ProviderEvent::ResponseResolved {
                    run_id: self.run_id(),
                    request_id,
                })
                .await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_server_request(&mut self, value: Value) -> Result<(), ()> {
        let rpc_id = value.get("id").and_then(RpcId::from_json).ok_or(())?;
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .ok_or(())?
            .to_owned();
        let params = value.get("params").ok_or(())?;
        if method == "currentTime/read" {
            let current_time = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|_| ())?
                .as_secs();
            return self
                .send_raw(json!({
                    "id": rpc_id.to_json(),
                    "result": {"currentTimeAt": current_time}
                }))
                .await;
        }

        let class = match method.as_str() {
            "item/commandExecution/requestApproval" => AttentionClass::CommandApproval,
            "item/fileChange/requestApproval" => AttentionClass::FileChangeApproval,
            "item/permissions/requestApproval" => AttentionClass::PermissionApproval,
            "item/tool/requestUserInput" => AttentionClass::UserInput,
            _ => {
                self.send_raw(json!({
                    "id": rpc_id.to_json(),
                    "error": {"code": -32601, "message": "request method is not supported"}
                }))
                .await?;
                return Err(());
            }
        };
        let thread_id = required_string(params, "/threadId")?;
        let turn_id = required_string(params, "/turnId")?;
        let item_id = required_string(params, "/itemId")?;
        let questions = if class == AttentionClass::UserInput {
            parse_questions(params)?
        } else {
            Vec::new()
        };
        let requested_action = questions
            .first()
            .map(|question| question.prompt.clone())
            .unwrap_or_else(|| requested_action(class.clone(), params));
        let request_id = rpc_id.audit_id();
        let attention = ProviderAttention {
            token: super::ResponseToken {
                rpc_id,
                method,
                request_id,
            },
            class,
            thread_id,
            turn_id,
            item_id,
            requested_action,
            questions,
        };
        self.emit(ProviderEvent::AttentionRequested {
            run_id: self.run_id(),
            attention,
        })
        .await;
        Ok(())
    }

    async fn send_provider_response(
        &mut self,
        token: super::ResponseToken,
        response: ProviderResponse,
    ) -> Result<(), ()> {
        let result = match (token.method.as_str(), response) {
            (
                "item/commandExecution/requestApproval"
                | "item/fileChange/requestApproval"
                | "item/permissions/requestApproval",
                ProviderResponse::Approve,
            ) => json!({"decision": "accept"}),
            (
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval",
                ProviderResponse::ApproveForSession,
            ) => json!({"decision": "acceptForSession"}),
            (
                "item/commandExecution/requestApproval"
                | "item/fileChange/requestApproval"
                | "item/permissions/requestApproval",
                ProviderResponse::Decline,
            ) => json!({"decision": "decline"}),
            ("item/tool/requestUserInput", ProviderResponse::Answers(answers)) => {
                let answers = answers
                    .into_iter()
                    .map(|(key, answers)| (key, json!({"answers": answers})))
                    .collect::<serde_json::Map<_, _>>();
                json!({"answers": answers})
            }
            _ => return Err(()),
        };
        self.send_raw(json!({"id": token.rpc_id.to_json(), "result": result}))
            .await
    }

    async fn send_request(
        &mut self,
        method: &str,
        params: Value,
        pending: PendingRequest,
    ) -> Result<(), ()> {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.checked_add(1).ok_or(())?;
        self.send_raw(json!({"id": id, "method": method, "params": params}))
            .await?;
        self.pending.insert(
            id,
            PendingCall {
                request: pending,
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
        stdin.write_all(&bytes).await.map_err(|_| ())?;
        stdin.flush().await.map_err(|_| ())
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

async fn sleep_until_request_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn required_string(value: &Value, pointer: &str) -> Result<String, ()> {
    let value = value.pointer(pointer).and_then(Value::as_str).ok_or(())?;
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(());
    }
    Ok(value.to_owned())
}

fn initialize_params() -> Value {
    json!({
        "clientInfo": {
            "name": "muxora",
            "title": "Muxora",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn thread_start_params(cwd: &std::path::Path, sandbox: super::SandboxAccess) -> Value {
    json!({
        "cwd": cwd,
        "approvalPolicy": "on-request",
        "sandbox": sandbox.codex_value(),
        "ephemeral": false
    })
}

fn thread_resume_params(thread_id: &str) -> Value {
    json!({"threadId": thread_id})
}

fn turn_start_params(thread_id: &str, input: String) -> Value {
    json!({
        "threadId": thread_id,
        "input": [{"type": "text", "text": input}]
    })
}

fn requested_action(class: AttentionClass, params: &Value) -> String {
    let raw = match class {
        AttentionClass::CommandApproval => params
            .get("command")
            .and_then(Value::as_str)
            .or_else(|| params.get("reason").and_then(Value::as_str))
            .unwrap_or("Run a command"),
        AttentionClass::FileChangeApproval => params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Apply file changes"),
        AttentionClass::PermissionApproval => params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Grant additional permissions"),
        AttentionClass::UserInput => "Answer a provider question",
    };
    bounded_text(raw)
}

fn parse_questions(params: &Value) -> Result<Vec<ProviderQuestion>, ()> {
    let values = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or(())?;
    if !(1..=4).contains(&values.len()) {
        return Err(());
    }
    values
        .iter()
        .map(|value| {
            let id = required_string(value, "/id")?;
            let prompt = required_string(value, "/question")?;
            let header = value
                .get("header")
                .and_then(Value::as_str)
                .map(bounded_text)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "Question".to_owned());
            let options = value
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    if options.len() > 8 {
                        return Err(());
                    }
                    options
                        .iter()
                        .map(|option| {
                            let label = required_string(option, "/label")?;
                            let description = option
                                .get("description")
                                .and_then(Value::as_str)
                                .map(bounded_text)
                                .unwrap_or_default();
                            Ok(ProviderQuestionOption { label, description })
                        })
                        .collect::<Result<Vec<_>, ()>>()
                })
                .transpose()?
                .unwrap_or_default();
            Ok(ProviderQuestion {
                id,
                header,
                prompt,
                options,
                multiple: value
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                custom_allowed: true,
            })
        })
        .collect()
}

fn bounded_text(value: &str) -> String {
    if value.len() <= MAX_VISIBLE_TEXT_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_VISIBLE_TEXT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> (Actor, mpsc::Receiver<ProviderEvent>) {
        let (events, rx) = mpsc::channel(8);
        let mut actor = Actor::new(PathBuf::from("unused"), events);
        actor.run_id = Some("run-1".to_owned());
        (actor, rx)
    }

    #[tokio::test]
    async fn exact_server_request_becomes_typed_attention_without_provider_payload_storage() {
        let (mut actor, mut events) = actor();
        actor
            .handle_server_request(json!({
                "id": "approval-rpc-1",
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "item-1",
                    "approvalId": "approval-1",
                    "command": "cargo test"
                }
            }))
            .await
            .unwrap();

        let ProviderEvent::AttentionRequested { run_id, attention } = events.recv().await.unwrap()
        else {
            panic!("expected attention event");
        };
        assert_eq!(run_id, "run-1");
        assert_eq!(attention.class, AttentionClass::CommandApproval);
        assert_eq!(attention.token.request_id(), "string:approval-rpc-1");
        assert_eq!(attention.requested_action, "cargo test");
        assert!(attention.questions.is_empty());
    }

    #[tokio::test]
    async fn user_input_request_preserves_exact_question_ids_and_choices() {
        let (mut actor, mut events) = actor();
        actor
            .handle_server_request(json!({
                "id": "question-rpc-1",
                "method": "item/tool/requestUserInput",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "item-1",
                    "questions": [{
                        "id": "database-id",
                        "header": "Database",
                        "question": "Which database?",
                        "options": [{"label": "Postgres", "description": "Relational"}]
                    }]
                }
            }))
            .await
            .unwrap();

        let ProviderEvent::AttentionRequested { attention, .. } = events.recv().await.unwrap()
        else {
            panic!("expected question attention");
        };
        assert_eq!(attention.requested_action, "Which database?");
        assert_eq!(attention.questions.len(), 1);
        assert_eq!(attention.questions[0].id, "database-id");
        assert_eq!(attention.questions[0].options[0].label, "Postgres");
    }

    #[tokio::test]
    async fn completed_notification_preserves_failure_instead_of_claiming_success() {
        let (mut actor, mut events) = actor();
        actor
            .handle_notification(&json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-1",
                    "turn": {"id": "turn-1", "status": "failed"}
                }
            }))
            .await
            .unwrap();

        assert_eq!(
            events.recv().await,
            Some(ProviderEvent::TurnCompleted {
                run_id: "run-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                outcome: TurnOutcome::Failed,
            })
        );
    }

    #[test]
    fn bounded_text_never_splits_utf8() {
        let input = "é".repeat(MAX_VISIBLE_TEXT_BYTES);
        let bounded = bounded_text(&input);
        assert!(bounded.len() <= MAX_VISIBLE_TEXT_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
    }

    #[test]
    fn lifecycle_requests_match_the_generated_protocol_contract() {
        assert_eq!(
            initialize_params(),
            json!({
                "clientInfo": {
                    "name": "muxora",
                    "title": "Muxora",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })
        );
        assert_eq!(
            thread_start_params(
                std::path::Path::new("/repo"),
                super::super::SandboxAccess::ReadOnly
            ),
            json!({
                "cwd": "/repo",
                "approvalPolicy": "on-request",
                "sandbox": "read-only",
                "ephemeral": false
            })
        );
        assert_eq!(
            thread_start_params(
                std::path::Path::new("/repo"),
                super::super::SandboxAccess::WorkspaceWriteConfirmed,
            )["sandbox"],
            "workspace-write"
        );
        assert_eq!(
            thread_resume_params("thread-1"),
            json!({"threadId": "thread-1"})
        );
        assert_eq!(
            turn_start_params("thread-1", "Do the work".to_owned()),
            json!({
                "threadId": "thread-1",
                "input": [{"type": "text", "text": "Do the work"}]
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actor_completes_a_real_jsonl_process_lifecycle() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-provider");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"session-live"}}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-live"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"turn":{"id":"turn-live","status":"completed"}}}'
      ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        spawn(Some(executable), command_rx, event_tx);
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-live".into(),
                cwd: directory.path().to_path_buf(),
                resume_session_id: None,
                initial_input: "Do the work".into(),
                sandbox: super::super::SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        let ready = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .unwrap();
        assert_eq!(
            ready,
            Some(ProviderEvent::Ready {
                run_id: "run-live".into(),
                session_id: "session-live".into(),
            })
        );
        let working = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .unwrap();
        assert_eq!(
            working,
            Some(ProviderEvent::Working {
                run_id: "run-live".into(),
                turn_id: "turn-live".into(),
            })
        );
        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .unwrap();
        assert_eq!(
            completed,
            Some(ProviderEvent::TurnCompleted {
                run_id: "run-live".into(),
                turn_id: "turn-live".into(),
                outcome: TurnOutcome::Completed,
            })
        );
        command_tx.send(ProviderCommand::Shutdown).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actor_times_out_a_provider_that_never_completes_handshake() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("stalled-provider");
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
                cwd: directory.path().to_path_buf(),
                resume_session_id: None,
                initial_input: String::new(),
                sandbox: super::super::SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .expect("actor timeout event");
        assert_eq!(
            event,
            Some(ProviderEvent::TransportFailed {
                run_id: "run-stalled".into(),
                reason: TransportFailure::Timeout,
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actor_resume_never_replays_an_initial_turn_when_input_is_empty() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("resume-provider");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"session-resumed"}}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' 'unexpected-turn-replay'
      ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let (command_tx, command_rx) = mpsc::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(4);
        spawn(Some(executable), command_rx, event_tx);
        command_tx
            .send(ProviderCommand::StartOrResume(StartOrResume {
                run_id: "run-resume".into(),
                cwd: directory.path().to_path_buf(),
                resume_session_id: Some("session-resumed".into()),
                initial_input: String::new(),
                sandbox: super::super::SandboxAccess::ReadOnly,
            }))
            .await
            .unwrap();

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
                .await
                .unwrap(),
            Some(ProviderEvent::Ready {
                run_id: "run-resume".into(),
                session_id: "session-resumed".into(),
            })
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), event_rx.recv())
                .await
                .is_err(),
            "resume with empty input must not emit a turn or protocol failure"
        );
        command_tx.send(ProviderCommand::Shutdown).await.unwrap();
    }
}
