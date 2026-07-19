//! Headless server mode — runs the nagi event loop without a real terminal.
//!
//! The server:
//! - Does not enter raw mode or read stdin
//! - Creates and listens on both `nagi.sock` (existing JSON API) and
//!   `nagi-client.sock` (new binary protocol)
//! - Initializes AppState and all PTYs from session restore or fresh state
//! - Runs the main event loop (drain events, drain API requests, scheduled tasks)
//! - Renders to a virtual ratatui Buffer in memory
//! - Accepts client connections on the client socket
//! - Streams frames to connected clients after each render
//! - Routes client input events through the existing input pipeline
//! - Continues running after client disconnect
//! - Handles stale socket cleanup, explicit server stop, minimum terminal size,
//!   and pane spawn failure during restore

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyModifiers, MouseEventKind};
use interprocess::local_socket::traits::Listener as _;
#[cfg(windows)]
use interprocess::local_socket::traits::Stream as _;
#[cfg(unix)]
use interprocess::local_socket::ListenerNonblockingMode;
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use base64::Engine;
use bytes::Bytes;

use crate::api;
use crate::app;
use crate::config;
use crate::events::AppEvent;
use crate::ipc::{
    bind_local_listener, remove_socket_file_if_owned, socket_file_identity, LocalListener,
    SocketFileIdentity,
};
use crate::protocol::{
    self, AttachScrollDirection, AttachScrollSource, FrameData, ServerMessage, MAX_FRAME_SIZE,
    MAX_GRAPHICS_FRAME_SIZE,
};
#[cfg(unix)]
use crate::server::client_accept::{
    accept_pending_client_connections, reject_pending_client_connections,
};
use crate::server::client_transport::ServerEvent;
use crate::server::clients::{
    events_include_interaction, latest_app_client, render_targets, terminal_stream_client_ids,
    ClientConnection, ClientConnectionMode, DeferredRender,
};
use crate::server::keybindings::{app_keybindings, apply_keybindings};
use crate::server::notifications::{
    should_forward_toast_to_clients, toast_message_from_state_change, toast_notify_kind,
};
use crate::server::socket_paths::{
    client_socket_path, prepare_socket_path, restrict_socket_permissions,
};
use crate::server::terminal_attach::paste_payload_for_runtime;

mod pane_graphics;

#[cfg(test)]
use pane_graphics::frame_pane_graphics_for_client;
use pane_graphics::RetainedGraphicsOutcome;

#[cfg(test)]
use crate::protocol::RenderEncoding;
#[cfg(test)]
use crate::server::client_transport::ClientWriter;
#[cfg(test)]
use std::fs;

const LIVE_HANDOFF_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(6);

fn wait_for_live_handoff_response_write(
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
) {
    let Some(response_write_complete) = response_write_complete else {
        return;
    };

    match response_write_complete.recv_timeout(LIVE_HANDOFF_RESPONSE_WRITE_TIMEOUT) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            warn!("timed out waiting for live handoff response write; old server exiting");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            warn!("live handoff response writer disconnected; old server exiting");
        }
    }
}

fn sound_notify_message(sound: crate::sound::Sound) -> &'static str {
    match sound {
        crate::sound::Sound::Done => "agent done",
        crate::sound::Sound::Request => "agent attention",
    }
}

fn notification_show_response_shown(response: &str) -> bool {
    let Ok(response) = serde_json::from_str::<api::schema::SuccessResponse>(response) else {
        return false;
    };
    matches!(
        response.result,
        api::schema::ResponseResult::NotificationShow { shown: true, .. }
    )
}

fn non_empty_body(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn encode_mission_error(request_id: &str, code: &str, message: &str) -> String {
    serde_json::to_string(&crate::api::schema::ErrorResponse {
        id: request_id.to_owned(),
        error: crate::api::schema::ErrorBody {
            code: code.to_owned(),
            message: message.to_owned(),
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"serialization_failed","message":"mission response serialization failed"}}"#
            .to_owned()
    })
}

fn encode_mission_run_started(
    request_id: &str,
    mission: crate::mission::store::MissionView,
) -> String {
    serde_json::to_string(&crate::api::schema::SuccessResponse {
        id: request_id.to_owned(),
        result: crate::api::schema::ResponseResult::MissionRunStarted {
            mission: crate::server::mission_bridge::mission_view(mission),
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"serialization_failed","message":"mission response serialization failed"}}"#
            .to_owned()
    })
}

fn mission_prompt(mission: &crate::mission::store::MissionView) -> String {
    let mut prompt = format!(
        "Objective:\n{}\n\nAcceptance criteria:\n",
        mission.objective
    );
    for (index, criterion) in mission.acceptance_criteria.iter().enumerate() {
        use std::fmt::Write as _;
        let _ = writeln!(prompt, "{}. {}", index + 1, criterion);
    }
    prompt
}

fn mission_handoff_prompt(artifact: &crate::api::schema::MissionHandoffArtifactV1) -> String {
    use std::fmt::Write as _;

    let mut prompt = format!(
        "Continue this existing Nagi mission from run `{}`.\n\nObjective:\n{}\n\nAcceptance criteria:\n",
        artifact.source_run_id, artifact.objective
    );
    for (index, criterion) in artifact.acceptance_criteria.iter().enumerate() {
        let _ = writeln!(prompt, "{}. {}", index + 1, criterion);
    }
    let _ = write!(
        prompt,
        "\nWorkspace handoff:\n- base: {}\n- head: {}\n- dirty: {}\n- digest: {}\n- changed paths:\n",
        artifact.base_revision,
        artifact.head_revision,
        artifact.diff.dirty,
        artifact.diff.workspace_digest,
    );
    if artifact.diff.changed_paths.is_empty() {
        prompt.push_str("  - none\n");
    } else {
        for path in &artifact.diff.changed_paths {
            let _ = writeln!(prompt, "  - {path}");
        }
    }
    if !artifact.decisions.is_empty() {
        prompt.push_str("\nAcknowledged decisions:\n");
        for decision in &artifact.decisions {
            let _ = writeln!(
                prompt,
                "- {}: {:?} ({:?})",
                decision.attention_id, decision.decision, decision.state
            );
        }
    }
    prompt.push_str(
        "\nRe-read the current workspace before editing. Hidden reasoning and provider session state were not transferred. Re-run required checks before declaring the mission complete.\n",
    );
    prompt
}

fn stable_runtime_id(domain: &str, values: &[&str]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"mission-runtime-id-v1\0");
    hasher.update(domain.as_bytes());
    for value in values {
        hasher.update([0]);
        hasher.update(value.as_bytes());
    }
    let digest = hasher.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{domain}:{encoded}")
}

fn project_resource_allocator() -> Result<crate::resources::ports::PortAllocator, String> {
    crate::resources::ports::PortAllocator::open(
        &crate::config::state_dir().join("project-resources"),
    )
    .map_err(|error| error.to_string())
}

fn configured_acp_endpoint(
    config: &crate::config::Config,
) -> Result<Option<crate::managed_provider::AcpEndpoint>, String> {
    let Some(config) = config.providers.acp.as_ref() else {
        return Ok(None);
    };
    let (executable, args) = config.endpoint()?;
    crate::managed_provider::AcpEndpoint::stdio(executable, args)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn provision_project_recipe(
    worktree: &Path,
    mission_id: &str,
    run_id: &str,
    execute_setup: bool,
) -> Result<(), String> {
    let Some(contract) =
        crate::project_recipe::load_contract(worktree).map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    if execute_setup {
        if let Some(setup) = &contract.setup {
            let result = crate::project_recipe::run_setup(worktree, setup)
                .map_err(|error| error.to_string())?;
            if !result.succeeded() {
                return Err(project_command_failure(&result));
            }
        }
    }
    let services = crate::resources::services::ServiceSet::start(
        project_resource_allocator()?,
        &contract,
        worktree,
        mission_id,
        run_id,
        crate::server::mission_bridge::now_millis(),
    )
    .map_err(|error| error.to_string())?;
    services.detach();
    Ok(())
}

fn project_command_failure(result: &crate::project_recipe::ProjectCommandResult) -> String {
    let details = if result.stderr.trim().is_empty() {
        result.stdout.trim()
    } else {
        result.stderr.trim()
    };
    let details = details.chars().take(2_048).collect::<String>();
    format!(
        "project {} failed with exit code {}{}",
        result.id,
        result
            .exit_code
            .map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
        if details.is_empty() {
            String::new()
        } else {
            format!(": {details}")
        }
    )
}

fn stop_project_services(mission_id: &str, run_id: &str) -> Result<(), String> {
    crate::resources::services::ServiceSet::stop_owner(
        &project_resource_allocator()?,
        mission_id,
        run_id,
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn run_project_cleanup(worktree: PathBuf) {
    let contract = match crate::project_recipe::load_contract(&worktree) {
        Ok(Some(contract)) => contract,
        Ok(None) => return,
        Err(error) => {
            warn!(path = %worktree.display(), err = %error, "project cleanup contract is unavailable");
            return;
        }
    };
    for (index, cleanup) in contract.cleanup.iter().enumerate() {
        match crate::project_recipe::run_cleanup(&worktree, index, cleanup) {
            Ok(result) if result.succeeded() => {}
            Ok(result) => {
                warn!(path = %worktree.display(), error = %project_command_failure(&result), "project cleanup command failed");
            }
            Err(error) => {
                warn!(path = %worktree.display(), err = %error, "project cleanup command could not run");
            }
        }
    }
}

fn wire_attention_decision(
    decision: crate::mission::attention::AttentionDecision,
) -> crate::api::schema::AttentionDecisionV1 {
    match decision {
        crate::mission::attention::AttentionDecision::ApproveOnce => {
            crate::api::schema::AttentionDecisionV1::ApproveOnce
        }
        crate::mission::attention::AttentionDecision::ApproveForSession => {
            crate::api::schema::AttentionDecisionV1::ApproveForSession
        }
        crate::mission::attention::AttentionDecision::AllowForMission => {
            crate::api::schema::AttentionDecisionV1::AllowForMission
        }
        crate::mission::attention::AttentionDecision::Deny => {
            crate::api::schema::AttentionDecisionV1::Deny
        }
        crate::mission::attention::AttentionDecision::Answer => {
            crate::api::schema::AttentionDecisionV1::Answer
        }
    }
}

fn wire_attention_failure(
    code: crate::mission::attention::ResponseFailureCode,
) -> crate::api::schema::AttentionFailureCodeV1 {
    match code {
        crate::mission::attention::ResponseFailureCode::Rejected => {
            crate::api::schema::AttentionFailureCodeV1::Rejected
        }
        crate::mission::attention::ResponseFailureCode::DisconnectedBeforeWrite => {
            crate::api::schema::AttentionFailureCodeV1::DisconnectedBeforeWrite
        }
        crate::mission::attention::ResponseFailureCode::Timeout => {
            crate::api::schema::AttentionFailureCodeV1::Timeout
        }
        crate::mission::attention::ResponseFailureCode::TransportClosed => {
            crate::api::schema::AttentionFailureCodeV1::TransportClosed
        }
    }
}

fn wire_attention_risk(
    risk: crate::mission::attention::AttentionRisk,
) -> crate::api::schema::AttentionRiskV1 {
    match risk {
        crate::mission::attention::AttentionRisk::Low => crate::api::schema::AttentionRiskV1::Low,
        crate::mission::attention::AttentionRisk::Medium => {
            crate::api::schema::AttentionRiskV1::Medium
        }
        crate::mission::attention::AttentionRisk::High => crate::api::schema::AttentionRiskV1::High,
        crate::mission::attention::AttentionRisk::Critical => {
            crate::api::schema::AttentionRiskV1::Critical
        }
    }
}

fn wire_mission_provider(
    provider: crate::mission::model::ProviderKind,
) -> crate::api::schema::MissionProvider {
    match provider {
        crate::mission::model::ProviderKind::Codex => crate::api::schema::MissionProvider::Codex,
        crate::mission::model::ProviderKind::ClaudeCode => {
            crate::api::schema::MissionProvider::ClaudeCode
        }
        crate::mission::model::ProviderKind::OpenCode => {
            crate::api::schema::MissionProvider::OpenCode
        }
        crate::mission::model::ProviderKind::Acp => crate::api::schema::MissionProvider::Acp,
    }
}

fn validate_managed_answers(
    answers: &std::collections::BTreeMap<String, Vec<String>>,
    questions: &[crate::managed_provider::ProviderQuestion],
) -> Result<(), ()> {
    const MAX_QUESTIONS: usize = 4;
    const MAX_ANSWERS_PER_QUESTION: usize = 8;
    const MAX_KEY_BYTES: usize = 1024;
    const MAX_ANSWER_BYTES: usize = 4 * 1024;
    const MAX_TOTAL_BYTES: usize = 32 * 1024;

    if questions.is_empty() || questions.len() > MAX_QUESTIONS || answers.len() != questions.len() {
        return Err(());
    }
    let mut total_bytes = 0_usize;
    for (key, values) in answers {
        if key.trim().is_empty()
            || key.len() > MAX_KEY_BYTES
            || key.contains('\0')
            || values.is_empty()
            || values.len() > MAX_ANSWERS_PER_QUESTION
        {
            return Err(());
        }
        total_bytes = total_bytes.checked_add(key.len()).ok_or(())?;
        for value in values {
            if value.trim().is_empty() || value.len() > MAX_ANSWER_BYTES || value.contains('\0') {
                return Err(());
            }
            total_bytes = total_bytes.checked_add(value.len()).ok_or(())?;
        }
    }
    for question in questions {
        let values = answers.get(&question.id).ok_or(())?;
        if !question.multiple && values.len() != 1 {
            return Err(());
        }
        if !question.custom_allowed
            && values.iter().any(|answer| {
                !question
                    .options
                    .iter()
                    .any(|option| option.label == answer.as_str())
            })
        {
            return Err(());
        }
    }
    (total_bytes <= MAX_TOTAL_BYTES).then_some(()).ok_or(())
}

// ---------------------------------------------------------------------------
// Loop event enum for the headless server event loop
// ---------------------------------------------------------------------------

/// Events that the headless server event loop can process.
enum LoopEvent {
    Timer,
    Internal(AppEvent),
    Api(Box<api::ApiRequestMessage>),
    ServerEvent(ServerEvent),
    Provider(crate::managed_provider::ProviderEvent),
    Proof(ProofWorkerEvent),
    RenderRequested,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum RenderImpact {
    #[default]
    None,
    Graphics,
    Full,
}

impl RenderImpact {
    fn merge(&mut self, other: Self) {
        *self = (*self).max(other);
    }
}

fn record_render_impact(source: &'static str, impact: RenderImpact) {
    let event = match (source, impact) {
        ("api_requests", RenderImpact::Graphics) => "graphics_render_cause.api_requests",
        ("api_requests", RenderImpact::Full) => "full_render_cause.api_requests",
        ("server_events", RenderImpact::Graphics) => "graphics_render_cause.server_events",
        ("server_events", RenderImpact::Full) => "full_render_cause.server_events",
        _ => return,
    };
    crate::render_prof::event(event);
}

fn rect_fits_frame(rect: Rect, frame: &FrameData) -> bool {
    rect.x.saturating_add(rect.width) <= frame.width
        && rect.y.saturating_add(rect.height) <= frame.height
}

fn apply_terminal_dirty_patch(
    frame: &mut FrameData,
    area: Rect,
    patch: crate::pane::TerminalDirtyPatch,
) -> bool {
    if !rect_fits_frame(area, frame) {
        return false;
    }
    let width = usize::from(frame.width);
    for (local_y, row_cells) in patch.rows {
        if local_y >= area.height || row_cells.len() != usize::from(area.width) {
            return false;
        }
        let frame_y = area.y + local_y;
        let start = usize::from(frame_y) * width + usize::from(area.x);
        let end = start + usize::from(area.width);
        if end > frame.cells.len() {
            return false;
        }
        frame.cells[start..end].clone_from_slice(&row_cells);
    }
    true
}

fn dirty_patch_intersects_hyperlinks(
    frame: &FrameData,
    area: Rect,
    patch: &crate::pane::TerminalDirtyPatch,
) -> bool {
    if frame.hyperlinks.is_empty() || !rect_fits_frame(area, frame) {
        return false;
    }
    let width = usize::from(frame.width);
    for (local_y, _) in &patch.rows {
        if *local_y >= area.height {
            return true;
        }
        let frame_y = area.y + *local_y;
        let start = usize::from(frame_y) * width + usize::from(area.x);
        let end = start + usize::from(area.width);
        if end > frame.cells.len() {
            return true;
        }
        if frame.cells[start..end]
            .iter()
            .any(|cell| cell.hyperlink.is_some())
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default shared runtime size (columns, rows) when no clients are attached.
const MIN_COLS: u16 = 80;
const MIN_ROWS: u16 = 24;

/// Timeout for in-flight API requests during shutdown.
#[allow(dead_code)]
const SHUTDOWN_API_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the idle headless loop wakes to poll the local listener for new
/// client connections.
///
/// The listener is non-blocking and not integrated into `tokio::select!`, so
/// a low-frequency wake is required to notice new thin-client attaches while
/// otherwise idle. Keep this much slower than the old resize-poll cadence to
/// avoid reintroducing the idle CPU spin.
const CLIENT_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Headless server
// ---------------------------------------------------------------------------

/// The headless server — runs the nagi event loop without a real terminal.
pub struct HeadlessServer {
    app: app::App,
    mission_runtime: crate::mission::runtime::MissionRuntime,
    managed_runs: HashMap<String, ManagedRun>,
    managed_provider_executable: Option<PathBuf>,
    acp_endpoint: Option<crate::managed_provider::AcpEndpoint>,
    provider_event_rx: mpsc::Receiver<crate::managed_provider::ProviderEvent>,
    provider_event_tx: mpsc::Sender<crate::managed_provider::ProviderEvent>,
    proof_event_rx: mpsc::Receiver<ProofWorkerEvent>,
    proof_event_tx: mpsc::Sender<ProofWorkerEvent>,
    pending_proofs: HashMap<String, PendingProofRun>,
    pending_mission_launches: HashMap<String, PendingMissionLaunch>,
    pending_project_launches: HashMap<String, PendingProjectLaunch>,
    #[cfg(unix)]
    api_tx: Option<api::ApiRequestSender>,
    // Kept on every platform so dropping HeadlessServer owns API server shutdown.
    #[cfg_attr(windows, allow(dead_code))]
    api_server: Option<api::ServerHandle>,
    #[cfg(unix)]
    client_listener: LocalListener,
    client_socket_path: PathBuf,
    client_socket_identity: SocketFileIdentity,
    clients: HashMap<u64, ClientConnection>,
    #[cfg(unix)]
    next_client_id: u64,
    /// The client currently driving the shared pane runtime size, theme, and input keybindings.
    foreground_client_id: Option<u64>,
    /// Server-owned keybindings, restored when foreground clients use server mode.
    server_keybindings: crate::config::LiveKeybindConfig,
    /// Full server config warning shown to clients that use server keybindings.
    server_config_diagnostic: Option<String>,
    /// Server config warning with keybinding diagnostics removed for local-keybinding clients.
    server_config_diagnostic_without_keybindings: Option<String>,
    /// Writable direct attach owner per terminal id string.
    terminal_attach_owners: HashMap<String, u64>,
    /// Monotonic activity counter used to pick the most recently active client.
    next_activity_stamp: u64,
    /// Shared pane runtime size derived from the foreground client,
    /// or MIN_COLS × MIN_ROWS when no clients are connected.
    effective_size: (u16, u16),
    /// Flag set when shutdown is initiated.
    shutting_down: bool,
    /// Flag set while exporting live PTYs to a replacement server.
    handoff_in_progress: bool,
    /// Imported panes get one app-safe resize nudge after the first client attaches.
    #[cfg(unix)]
    pending_handoff_repaint_nudge: bool,
    /// Flag set by Ctrl+C or `server stop` signal.
    should_quit: Arc<AtomicBool>,
    /// Channel for receiving server events from client connection threads.
    server_event_rx: mpsc::Receiver<ServerEvent>,
    /// Sender for server events (cloned for each client thread).
    server_event_tx: mpsc::Sender<ServerEvent>,
}

struct ManagedRun {
    mission_id: String,
    provider: crate::mission::model::ProviderKind,
    recovered: bool,
    execute_declared_checks: bool,
    execute_project_recipe: bool,
    handle: crate::managed_provider::ManagedProviderHandle,
    lease: crate::mission::claims::WorktreeLease,
    responses: HashMap<String, AvailableManagedResponse>,
    inflight_responses: HashMap<String, PendingManagedResponse>,
}

struct PendingProofRun {
    mission_id: String,
    lease: crate::mission::claims::WorktreeLease,
    cancelled: Arc<AtomicBool>,
}

struct ProofWorkerEvent {
    run_id: String,
    result: Result<crate::mission::evidence_pack::EvidencePack, String>,
}

struct PendingMissionLaunch {
    request: crate::app::state::NewMissionLaunchRequest,
    worktree_response: std::sync::mpsc::Receiver<String>,
}

struct PendingProjectLaunch {
    request: crate::app::state::NewMissionLaunchRequest,
    worktree_path: String,
    recipe_response: std::sync::mpsc::Receiver<Result<(), String>>,
}

#[derive(Clone)]
#[allow(
    dead_code,
    reason = "response routes are captured now but consumed only after interactive consent"
)]
struct AvailableManagedResponse {
    token: crate::managed_provider::ResponseToken,
    class: crate::managed_provider::AttentionClass,
    session_id: String,
    requested_action: String,
    questions: Vec<crate::managed_provider::ProviderQuestion>,
    created_at_millis: u64,
}

#[derive(Clone)]
struct PendingManagedResponse {
    attention_id: String,
    key: crate::mission::store::ResponseAttemptKey,
}

fn apply_terminal_attach_scroll(
    runtime: &crate::terminal::TerminalRuntime,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
    lines: u16,
    column: Option<u16>,
    row: Option<u16>,
    modifiers: u8,
) -> Result<(), String> {
    let wheel_kind = match direction {
        AttachScrollDirection::Up => MouseEventKind::ScrollUp,
        AttachScrollDirection::Down => MouseEventKind::ScrollDown,
    };
    if let AttachScrollSource::PageKey { input } = source {
        let host_scroll = runtime
            .input_state()
            .is_some_and(crate::pane::InputState::plain_page_keys_use_host_scrollback);
        if host_scroll {
            match direction {
                AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
                AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
            }
            return Ok(());
        }
        return apply_terminal_attach_input(runtime, input);
    }

    match runtime.wheel_routing() {
        Some(crate::pane::WheelRouting::MouseReport) => {
            runtime.scroll_reset();
            let column = column.unwrap_or(0);
            let row = row.unwrap_or(0);
            let Some(bytes) = runtime.encode_mouse_wheel(
                wheel_kind,
                column,
                row,
                KeyModifiers::from_bits_truncate(modifiers),
            ) else {
                return Err(format!(
                    "failed to encode terminal attach mouse wheel event: {wheel_kind:?}"
                ));
            };
            runtime
                .try_send_bytes(Bytes::from(bytes))
                .map_err(|err| format!("terminal attach mouse wheel input failed: {err}"))?;
        }
        Some(crate::pane::WheelRouting::AlternateScroll) => {
            runtime.scroll_reset();
            let Some(bytes) = runtime.encode_alternate_scroll(wheel_kind) else {
                return Ok(());
            };
            runtime
                .try_send_bytes(Bytes::from(bytes))
                .map_err(|err| format!("terminal attach alternate scroll input failed: {err}"))?;
        }
        Some(crate::pane::WheelRouting::HostScroll) | None => match direction {
            AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
            AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
        },
    }
    Ok(())
}

fn apply_terminal_attach_input(
    runtime: &crate::terminal::TerminalRuntime,
    data: Vec<u8>,
) -> Result<(), String> {
    runtime.scroll_reset();
    runtime
        .try_send_bytes(Bytes::from(data))
        .map_err(|err| format!("terminal attach input failed: {err}"))
}

#[cfg(windows)]
fn spawn_windows_client_accept_thread(
    listener: LocalListener,
    should_quit: Arc<AtomicBool>,
    server_event_tx: mpsc::Sender<ServerEvent>,
) {
    std::thread::spawn(move || {
        let mut next_client_id = 1_u64;
        while !should_quit.load(Ordering::Acquire) {
            let stream = match listener.accept() {
                Ok(stream) => stream,
                Err(err) => {
                    if should_quit.load(Ordering::Acquire) {
                        break;
                    }
                    error!(err = %err, "client listener accept failed");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };

            let client_id = next_client_id;
            next_client_id = next_client_id.saturating_add(1);

            if let Err(err) = stream.set_nonblocking(true) {
                warn!(err = %err, "failed to set client stream nonblocking");
                continue;
            }

            let should_quit = should_quit.clone();
            let server_event_tx = server_event_tx.clone();
            std::thread::spawn(move || {
                if let Err(err) = crate::server::client_transport::handle_client_handshake(
                    stream,
                    client_id,
                    &server_event_tx,
                    &should_quit,
                ) {
                    debug!(client_id, err = %err, "client handshake failed");
                }
            });
        }
    });
}

impl HeadlessServer {
    /// Creates and starts the headless server.
    ///
    /// This:
    /// 1. Prepares the client socket path (cleans up stale sockets)
    /// 2. Binds the client socket listener
    /// 3. Returns the server ready to run
    pub fn new(
        app: app::App,
        config_diagnostics: &[String],
        api_tx: Option<api::ApiRequestSender>,
        api_server: Option<api::ServerHandle>,
    ) -> io::Result<Self> {
        #[cfg(unix)]
        let mission_runtime = crate::mission::runtime::MissionRuntime::open_owned(
            &crate::session::data_dir(),
            &crate::config::state_dir().join("mission-worktree-locks"),
        )
        .map_err(io::Error::other)?;
        #[cfg(not(unix))]
        let mission_runtime = crate::mission::runtime::MissionRuntime::disabled();
        let mut server = Self::new_with_mission_runtime(
            app,
            config_diagnostics,
            api_tx,
            api_server,
            mission_runtime,
        )?;
        server.acp_endpoint = configured_acp_endpoint(&crate::config::Config::load().config)
            .unwrap_or_else(|error| {
                warn!(err = %error, "ACP provider configuration is invalid");
                None
            });
        Ok(server)
    }

    fn new_with_mission_runtime(
        app: app::App,
        config_diagnostics: &[String],
        api_tx: Option<api::ApiRequestSender>,
        api_server: Option<api::ServerHandle>,
        mission_runtime: crate::mission::runtime::MissionRuntime,
    ) -> io::Result<Self> {
        let client_path = client_socket_path();
        prepare_socket_path(&client_path)?;

        let listener = bind_local_listener(&client_path)?;
        restrict_socket_permissions(&client_path)?;
        let client_socket_identity = socket_file_identity(&client_path)?;
        info!(path = %client_path.display(), "client protocol socket listening");

        // Set non-blocking on Unix so we can poll it from the event loop.
        #[cfg(unix)]
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;

        let should_quit = Arc::new(AtomicBool::new(false));

        // Channel for server events from client threads.
        let (server_event_tx, server_event_rx) = mpsc::channel(64);
        let (provider_event_tx, provider_event_rx) = mpsc::channel(64);
        let (proof_event_tx, proof_event_rx) = mpsc::channel(8);
        #[cfg(windows)]
        spawn_windows_client_accept_thread(listener, should_quit.clone(), server_event_tx.clone());

        let server_keybindings = app_keybindings(&app);
        let (server_config_diagnostic, server_config_diagnostic_without_keybindings) =
            server_config_diagnostic_summaries(config_diagnostics);
        #[cfg(not(unix))]
        let _ = api_tx;
        let mut server = Self {
            app,
            mission_runtime,
            managed_runs: HashMap::new(),
            managed_provider_executable: None,
            acp_endpoint: None,
            provider_event_rx,
            provider_event_tx,
            proof_event_rx,
            proof_event_tx,
            pending_proofs: HashMap::new(),
            pending_mission_launches: HashMap::new(),
            pending_project_launches: HashMap::new(),
            #[cfg(unix)]
            api_tx,
            api_server,
            #[cfg(unix)]
            client_listener: listener,
            client_socket_path: client_path,
            client_socket_identity,
            clients: HashMap::new(),
            #[cfg(unix)]
            next_client_id: 1,
            foreground_client_id: None,
            server_keybindings,
            server_config_diagnostic,
            server_config_diagnostic_without_keybindings,
            terminal_attach_owners: HashMap::new(),
            next_activity_stamp: 1,
            effective_size: (MIN_COLS, MIN_ROWS),
            shutting_down: false,
            handoff_in_progress: false,
            #[cfg(unix)]
            pending_handoff_repaint_nudge: false,
            should_quit,
            server_event_rx,
            server_event_tx,
        };
        server.sync_mission_projection();
        Ok(server)
    }

    /// Runs the headless server event loop until shutdown.
    ///
    /// This is the server's main loop — analogous to `App::run()` but without
    /// a real terminal. It:
    /// - Drains internal events (pane death, state changes)
    /// - Drains API requests (from the JSON socket)
    /// - Accepts new client connections
    /// - Reads client messages and routes input
    /// - Handles scheduled tasks (resize poll, animation, session save, etc.)
    /// - Renders virtually and streams frames to clients
    pub async fn run(&mut self) -> io::Result<()> {
        crate::logging::startup("server");
        self.recover_managed_runs();
        self.sync_mission_projection();

        // Register SIGINT handler for graceful shutdown.
        let should_quit = self.should_quit.clone();
        let quit_notify = self.server_event_tx.clone();
        ctrlc_handler(should_quit, quit_notify);

        // No input_rx needed — server doesn't read stdin.
        // We use None for input_rx so the event loop doesn't try to read from stdin.
        self.app.input_rx = None;

        let mut needs_render = true;
        let mut needs_full_render = true;
        let mut needs_graphics_render = false;

        loop {
            crate::render_prof::event("loop.tick");
            crate::render_prof::flush_if_due();
            self.app.reap_finished_custom_commands();

            // If shutdown has been initiated, complete it and exit.
            if self.shutting_down {
                self.complete_shutdown()?;
                break;
            }

            // Check if we should start shutting down.
            if self.app.state.should_quit || self.should_quit.load(Ordering::Acquire) {
                self.initiate_shutdown();
                continue;
            }

            // 1. Check render_dirty flag from PTY reader tasks.
            if self.app.render_dirty.load(Ordering::Acquire) {
                needs_render = true;
                crate::render_prof::event("render.request.pty_dirty");
            }
            let terminal_title_changed = self.app.sync_terminal_titles();
            if terminal_title_changed && self.app.terminal_title_sidebar_configured() {
                needs_render = true;
                needs_full_render = true;
                crate::render_prof::event("full_render_cause.terminal_title");
            }

            // 2. Drain a bounded internal-event batch. API handlers perform an
            // exhaustive forwarding-aware drain before reading pane/runtime state.
            if self.drain_internal_events_with_forwarding() {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
                crate::render_prof::event("full_render_cause.internal_events");
            }
            if self.app.expire_due_metadata(Instant::now()) {
                needs_render = true;
                needs_full_render = true;
                crate::render_prof::event("full_render_cause.metadata_expiry");
            }

            // 3. Drain API requests.
            if self.pane_graphics_runtime_active() {
                let api_impact = self.drain_api_requests_with_render_impact();
                record_render_impact("api_requests", api_impact);
                match api_impact {
                    RenderImpact::None => {}
                    RenderImpact::Graphics => {
                        needs_render = true;
                        needs_graphics_render = true;
                    }
                    RenderImpact::Full => {
                        needs_render = true;
                        needs_full_render = true;
                        needs_graphics_render = false;
                    }
                }
            } else if self.drain_api_requests_with_shutdown_check() {
                needs_render = true;
                needs_full_render = true;
                crate::render_prof::event("full_render_cause.api_requests");
            }

            self.app.sync_focus_events();
            self.app.sync_session_save_schedule();

            // 4. Accept new client connections.
            self.accept_client_connections()?;

            // 5. Drain server events from client threads.
            if self.pane_graphics_runtime_active() {
                let server_impact = self.drain_server_events_with_render_impact();
                record_render_impact("server_events", server_impact);
                match server_impact {
                    RenderImpact::None => {}
                    RenderImpact::Graphics => {
                        needs_render = true;
                        needs_graphics_render = true;
                    }
                    RenderImpact::Full => {
                        needs_render = true;
                        needs_full_render = true;
                        needs_graphics_render = false;
                    }
                }
            } else if self.drain_server_events() {
                needs_render = true;
                needs_full_render = true;
                crate::render_prof::event("full_render_cause.server_events");
            }

            // 6. Handle scheduled tasks.
            let now = Instant::now();
            if self.handle_scheduled_tasks_headless(now, needs_render) {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
                crate::render_prof::event("full_render_cause.scheduled_tasks");
            }

            if self.handle_deferred_requests_headless() {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
            }

            if latest_app_client(&self.clients).is_some() && self.app.ensure_default_workspace() {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
                crate::render_prof::event("full_render_cause.default_workspace");
            }

            self.cancel_inactive_pane_graphics_streams();

            self.drain_client_config_reload_request();
            self.stream_host_mouse_capture_mode();

            self.app.sync_headless_animation_timer(now);

            // 7. Render virtually and stream frames.
            if needs_render && self.app.can_render_now(now) {
                crate::render_prof::event("render.attempt");
                let pty_dirty = self.app.render_dirty.swap(false, Ordering::AcqRel);
                if pty_dirty {
                    crate::render_prof::event("render.attempt.pty_dirty");
                }
                if needs_full_render {
                    crate::render_prof::event("retained_gate.needs_full_render");
                } else if !pty_dirty {
                    crate::render_prof::event("retained_gate.not_pty_dirty");
                }
                let mut deferred_graphics = false;
                let rendered_retained = if needs_graphics_render && !needs_full_render && !pty_dirty
                {
                    match self.render_retained_graphics_update_and_stream() {
                        RetainedGraphicsOutcome::Sent => true,
                        RetainedGraphicsOutcome::Deferred => {
                            deferred_graphics = true;
                            false
                        }
                        RetainedGraphicsOutcome::Fallback => false,
                    }
                } else {
                    pty_dirty && !needs_full_render && self.render_retained_pty_update_and_stream()
                };
                if deferred_graphics {
                    needs_render = false;
                    continue;
                }
                if !rendered_retained {
                    crate::render_prof::event("full_render.invoke");
                    self.render_and_stream();
                }
                self.app.last_render_at = Some(now);
                needs_render = false;
                needs_full_render = false;
                needs_graphics_render = false;
                continue;
            }

            // 8. Wait for next event.
            let next_deadline = self
                .app
                .next_headless_loop_deadline_with_git_refresh(
                    now,
                    needs_render,
                    self.has_app_client(),
                )
                .map(|deadline| deadline.min(now + CLIENT_ACCEPT_POLL_INTERVAL))
                .or(Some(now + CLIENT_ACCEPT_POLL_INTERVAL));
            let event = {
                tokio::select! {
                    maybe_api = self.app.api_rx.recv() => match maybe_api {
                        Some(msg) => LoopEvent::Api(Box::new(msg)),
                        None => LoopEvent::Timer,
                    },
                    maybe_ev = self.app.event_rx.recv() => match maybe_ev {
                        Some(ev) => LoopEvent::Internal(ev),
                        None => LoopEvent::Timer,
                    },
                    maybe_server_ev = self.server_event_rx.recv() => match maybe_server_ev {
                        Some(ev) => LoopEvent::ServerEvent(ev),
                        None => LoopEvent::Timer,
                    },
                    maybe_provider_ev = self.provider_event_rx.recv() => match maybe_provider_ev {
                        Some(ev) => LoopEvent::Provider(ev),
                        None => LoopEvent::Timer,
                    },
                    maybe_proof_ev = self.proof_event_rx.recv() => match maybe_proof_ev {
                        Some(ev) => LoopEvent::Proof(ev),
                        None => LoopEvent::Timer,
                    },
                    _ = sleep_until_or_pending(next_deadline) => LoopEvent::Timer,
                    _ = self.app.render_notify.notified() => LoopEvent::RenderRequested,
                }
            };

            match event {
                LoopEvent::Timer => {}
                LoopEvent::Internal(ev) => {
                    if self.handle_internal_event_with_forwarding(ev) {
                        needs_render = true;
                        needs_full_render = true;
                        needs_graphics_render = false;
                    }
                }
                LoopEvent::Api(msg) => {
                    if self.pane_graphics_runtime_active() {
                        let impact = self.handle_api_request_with_render_impact(*msg);
                        record_render_impact("api_requests", impact);
                        match impact {
                            RenderImpact::None => {}
                            RenderImpact::Graphics => {
                                needs_render = true;
                                needs_graphics_render = true;
                            }
                            RenderImpact::Full => {
                                needs_render = true;
                                needs_full_render = true;
                                needs_graphics_render = false;
                            }
                        }
                    } else if self.handle_api_request_with_shutdown_check(*msg) {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::ServerEvent(ev) => {
                    if self.pane_graphics_runtime_active() {
                        let impact = self.handle_server_event_with_render_impact(ev);
                        record_render_impact("server_events", impact);
                        match impact {
                            RenderImpact::None => {}
                            RenderImpact::Graphics => {
                                needs_render = true;
                                needs_graphics_render = true;
                            }
                            RenderImpact::Full => {
                                needs_render = true;
                                needs_full_render = true;
                                needs_graphics_render = false;
                            }
                        }
                    } else if self.handle_server_event(ev) {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::Provider(event) => {
                    if self.handle_provider_event(event) {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::Proof(event) => {
                    if self.handle_proof_event(event) {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::RenderRequested => {
                    if self.app.render_dirty.load(Ordering::Acquire) {
                        needs_render = true;
                    }
                }
            }
        }

        // Save session on exit.
        if !self.app.no_session {
            self.app.save_session_now();
        }

        info!("headless server exiting");
        Ok(())
    }

    fn handle_deferred_requests_headless(&mut self) -> bool {
        let mut needs_render = false;

        if self.app.state.request_complete_onboarding {
            self.app.state.request_complete_onboarding = false;
            self.app.complete_onboarding_to_mission();
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_onboarding");
        }

        if self.app.state.request_new_workspace {
            self.app.state.request_new_workspace = false;
            let response = self.headless_workspace_create("headless.workspace.create", None, None);
            if let Err(error) = response {
                error!(
                    code = %error.code,
                    message = %error.message,
                    "failed to create workspace"
                );
            }
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_new_workspace");
        }

        if self.app.state.request_new_tab {
            self.app.state.request_new_tab = false;
            let label = self.app.state.requested_new_tab_name.take();
            let response = self.headless_tab_create("headless.tab.create", label);
            if let Err(error) = response {
                error!(
                    code = %error.code,
                    message = %error.message,
                    "failed to create tab"
                );
            }
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_new_tab");
        }

        if let Some(ws_idx) = self.app.state.request_new_linked_worktree.take() {
            self.app.open_new_linked_worktree_dialog(ws_idx);
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_dialog");
        }

        if let Some(ws_idx) = self.app.state.request_open_existing_worktree.take() {
            self.app.open_existing_worktree_dialog(ws_idx);
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_dialog");
        }

        if let Some(cwd) = self.app.state.request_new_workspace_cwd.take() {
            let response = self.headless_workspace_create(
                "headless.workspace.create_cwd",
                Some(cwd.display().to_string()),
                None,
            );
            if let Err(error) = response {
                error!(
                    code = %error.code,
                    message = %error.message,
                    "failed to create workspace at requested cwd"
                );
                self.app.state.mode = app::Mode::Navigate;
            }
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_workspace_cwd");
        }

        if let Some(ws_idx) = self.app.state.request_remove_linked_worktree.take() {
            self.app.open_remove_linked_worktree_confirmation(ws_idx);
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_dialog");
        }

        if self.app.state.request_submit_worktree_create {
            self.app.state.request_submit_worktree_create = false;
            self.app.submit_worktree_create_via_api();
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_submit");
        }

        if self.app.state.request_submit_worktree_open {
            self.app.state.request_submit_worktree_open = false;
            self.app.submit_worktree_open_via_api();
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_submit");
        }

        if self.app.state.request_submit_worktree_remove {
            self.app.state.request_submit_worktree_remove = false;
            self.app.submit_worktree_remove_via_api();
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_submit");
        }

        if self.app.state.request_reload_config {
            self.app.state.request_reload_config = false;
            self.reload_server_config(true);
            needs_render = true;
            crate::render_prof::event("full_render_cause.config_reload");
        }

        if let Some(params) = self.app.state.request_attention_response.take() {
            let response = self.handle_mission_respond_authorized("tui.mission.respond", &params);
            self.app.state.attention_error =
                serde_json::from_str::<api::schema::ErrorResponse>(&response)
                    .ok()
                    .map(|response| response.error.message);
            self.sync_mission_projection();
            needs_render = true;
            crate::render_prof::event("full_render_cause.attention_response");
        }

        if let Some(target) = self.app.state.request_mission_close.take() {
            let response = self.handle_mission_close_api("tui.mission.close", &target);
            self.app.state.mission_action_error =
                serde_json::from_str::<api::schema::ErrorResponse>(&response)
                    .ok()
                    .map(|response| response.error.message);
            self.sync_mission_projection();
            needs_render = true;
            crate::render_prof::event("full_render_cause.mission_close");
        }

        if let Some(params) = self.app.state.request_mission_handoff_preview.take() {
            let method = api::schema::Method::MissionHandoffPreview(params.clone());
            let response = crate::server::mission_bridge::handle(
                &mut self.mission_runtime,
                "tui.mission.handoff.preview",
                &method,
            )
            .map(|outcome| outcome.response)
            .unwrap_or_else(|| {
                encode_mission_error(
                    "tui.mission.handoff.preview",
                    "feature_unavailable",
                    "mission handoff preview is unavailable",
                )
            });
            if let Some(draft) = self.app.state.mission_handoff.as_mut().filter(|draft| {
                draft.mission_id == params.mission_id && draft.target_provider == params.to
            }) {
                match serde_json::from_str::<api::schema::SuccessResponse>(&response) {
                    Ok(api::schema::SuccessResponse {
                        result: api::schema::ResponseResult::MissionHandoffPreview { artifact },
                        ..
                    }) => {
                        draft.artifact = Some(artifact);
                        draft.error = None;
                    }
                    _ => {
                        draft.artifact = None;
                        draft.error = serde_json::from_str::<api::schema::ErrorResponse>(&response)
                            .ok()
                            .map(|response| response.error.message)
                            .or_else(|| Some("Handoff preview failed".into()));
                    }
                }
                draft.loading = false;
            }
            needs_render = true;
            crate::render_prof::event("full_render_cause.mission_handoff_preview");
        }

        if let Some(request) = self.app.state.request_mission_handoff_start.take() {
            let response = self.handle_mission_handoff_start(
                "tui.mission.handoff.start",
                &request.params,
                request.workspace_write_confirmed,
            );
            if let Ok(success) = serde_json::from_str::<api::schema::SuccessResponse>(&response) {
                if matches!(
                    success.result,
                    api::schema::ResponseResult::MissionRunStarted { .. }
                ) {
                    self.app.state.mission_handoff = None;
                    self.app.state.mode = app::Mode::MissionInspector;
                    self.app.state.mission_action_error = None;
                }
            } else if let Some(draft) = self.app.state.mission_handoff.as_mut() {
                draft.loading = false;
                draft.error = serde_json::from_str::<api::schema::ErrorResponse>(&response)
                    .ok()
                    .map(|response| response.error.message)
                    .or_else(|| Some("Handoff start failed".into()));
            }
            self.sync_mission_projection();
            needs_render = true;
            crate::render_prof::event("full_render_cause.mission_handoff_start");
        }

        if let Some(request) = self.app.state.request_new_mission.take() {
            self.begin_new_mission_launch(request);
            needs_render = true;
            crate::render_prof::event("full_render_cause.mission_provision");
        }

        if self.finish_pending_mission_launches() {
            needs_render = true;
            crate::render_prof::event("full_render_cause.mission_launch");
        }

        needs_render
    }

    fn begin_new_mission_launch(&mut self, request: crate::app::state::NewMissionLaunchRequest) {
        use crate::api::schema::{Method, Request, WorktreeCreateParams};

        let mission_id = request.create.mission_id.clone();
        if self.pending_mission_launches.contains_key(&mission_id)
            || self.pending_project_launches.contains_key(&mission_id)
        {
            self.app.state.mission_action_error =
                Some("This mission is already being provisioned".into());
            return;
        }
        for method in [
            Method::MissionCreate(request.create.clone()),
            Method::MissionConfigure(request.configure.clone()),
        ] {
            let Some(outcome) = crate::server::mission_bridge::handle(
                &mut self.mission_runtime,
                "tui.mission.provision",
                &method,
            ) else {
                self.app.state.mission_action_error =
                    Some("Mission provisioning method is unavailable".into());
                return;
            };
            if let Ok(error) = serde_json::from_str::<api::schema::ErrorResponse>(&outcome.response)
            {
                self.app.state.mission_action_error = Some(error.error.message);
                self.sync_mission_projection();
                return;
            }
        }
        self.sync_mission_projection();

        let (respond_to, worktree_response) = std::sync::mpsc::channel();
        let worktree_request = Request {
            id: format!("tui.mission.worktree.{mission_id}"),
            method: Method::WorktreeCreate(WorktreeCreateParams {
                workspace_id: None,
                cwd: Some(request.create.repository_path.clone()),
                branch: Some(request.branch.clone()),
                base: Some("HEAD".into()),
                path: None,
                label: Some(request.create.title.clone()),
                focus: false,
            }),
        };
        if !self
            .app
            .handle_deferred_worktree_api_request(worktree_request, respond_to)
        {
            self.app.state.mission_action_error =
                Some("Worktree provisioning is unavailable".into());
            return;
        }
        self.pending_mission_launches.insert(
            mission_id,
            PendingMissionLaunch {
                request,
                worktree_response,
            },
        );
    }

    fn finish_pending_mission_launches(&mut self) -> bool {
        let worktrees = self
            .pending_mission_launches
            .iter()
            .filter_map(|(mission_id, pending)| match pending.worktree_response.try_recv() {
                Ok(response) => Some((mission_id.clone(), response)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((
                    mission_id.clone(),
                    r#"{"id":"","error":{"code":"worktree_create_failed","message":"worktree provisioning channel closed"}}"#.to_owned(),
                )),
            })
            .collect::<Vec<_>>();
        let recipes = self
            .pending_project_launches
            .iter()
            .filter_map(
                |(mission_id, pending)| match pending.recipe_response.try_recv() {
                    Ok(result) => Some((mission_id.clone(), result)),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((
                        mission_id.clone(),
                        Err("project recipe worker channel closed".to_owned()),
                    )),
                },
            )
            .collect::<Vec<_>>();
        if worktrees.is_empty() && recipes.is_empty() {
            return false;
        }

        for (mission_id, response) in worktrees {
            let Some(pending) = self.pending_mission_launches.remove(&mission_id) else {
                continue;
            };
            let worktree_path = serde_json::from_str::<api::schema::SuccessResponse>(&response)
                .ok()
                .and_then(|response| match response.result {
                    api::schema::ResponseResult::WorktreeCreated { worktree, .. } => {
                        Some(worktree.path)
                    }
                    _ => None,
                });
            let Some(worktree_path) = worktree_path else {
                self.app.state.mission_action_error = serde_json::from_str::<
                    api::schema::ErrorResponse,
                >(&response)
                .ok()
                .map(|response| response.error.message)
                .or_else(|| Some("Worktree provisioning returned an invalid response".into()));
                continue;
            };
            if pending.request.start.execute_project_recipe {
                let (sender, recipe_response) = std::sync::mpsc::channel();
                let recipe_path = worktree_path.clone();
                let mission_id_for_worker = pending.request.start.mission_id.clone();
                let run_id = pending.request.start.run_id.clone();
                std::thread::spawn(move || {
                    let result = provision_project_recipe(
                        Path::new(&recipe_path),
                        &mission_id_for_worker,
                        &run_id,
                        true,
                    );
                    let _ = sender.send(result);
                });
                self.pending_project_launches.insert(
                    mission_id,
                    PendingProjectLaunch {
                        request: pending.request,
                        worktree_path,
                        recipe_response,
                    },
                );
            } else {
                self.complete_pending_mission_launch(mission_id, pending.request, worktree_path);
            }
        }
        for (mission_id, result) in recipes {
            let Some(pending) = self.pending_project_launches.remove(&mission_id) else {
                continue;
            };
            match result {
                Ok(()) => self.complete_pending_mission_launch(
                    mission_id,
                    pending.request,
                    pending.worktree_path,
                ),
                Err(error) => self.app.state.mission_action_error = Some(error),
            }
        }
        self.sync_mission_projection();
        true
    }

    fn complete_pending_mission_launch(
        &mut self,
        mission_id: String,
        mut request: crate::app::state::NewMissionLaunchRequest,
        worktree_path: String,
    ) {
        request.start.worktree_path = Some(worktree_path);
        let response = self.handle_mission_start(
            "tui.mission.start",
            &request.start,
            request.workspace_write_confirmed,
        );
        if let Ok(error) = serde_json::from_str::<api::schema::ErrorResponse>(&response) {
            if request.start.execute_project_recipe {
                let _ = stop_project_services(&request.start.mission_id, &request.start.run_id);
            }
            self.app.state.mission_action_error = Some(error.error.message);
        } else {
            self.app.state.mission_action_error = None;
            self.app.state.selected_mission_id = Some(mission_id);
            self.app.state.mode = app::Mode::MissionInspector;
        }
    }

    fn headless_workspace_create(
        &mut self,
        id: &'static str,
        cwd: Option<String>,
        label: Option<String>,
    ) -> Result<(), api::schema::ErrorBody> {
        self.dispatch_headless_runtime_mutation(
            id,
            api::schema::Method::WorkspaceCreate(api::schema::WorkspaceCreateParams {
                cwd,
                focus: true,
                label,
                env: Default::default(),
            }),
        )
    }

    fn headless_tab_create(
        &mut self,
        id: &'static str,
        label: Option<String>,
    ) -> Result<(), api::schema::ErrorBody> {
        self.dispatch_headless_runtime_mutation(
            id,
            api::schema::Method::TabCreate(api::schema::TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: true,
                label,
                env: Default::default(),
            }),
        )
    }

    fn dispatch_headless_runtime_mutation(
        &mut self,
        id: &'static str,
        method: api::schema::Method,
    ) -> Result<(), api::schema::ErrorBody> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        self.handle_api_request_with_shutdown_check_inner(
            api::ApiRequestMessage {
                request: api::schema::Request {
                    id: id.to_string(),
                    method,
                },
                respond_to,
                response_write_complete: None,
            },
            true,
        );
        match response_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(response) => serde_json::from_str::<api::schema::ErrorResponse>(&response)
                .map(|response| Err(response.error))
                .unwrap_or(Ok(())),
            Err(err) => Err(api::schema::ErrorBody {
                code: "internal_error".into(),
                message: format!("headless runtime mutation response failed: {err}"),
            }),
        }
    }

    fn allocate_activity_stamp(&mut self) -> u64 {
        let stamp = self.next_activity_stamp;
        self.next_activity_stamp = self.next_activity_stamp.saturating_add(1);
        stamp
    }

    fn resize_shared_runtime_to_effective_size(&mut self) {
        self.resize_shared_runtime_to_effective_size_with_pending_agent_resumes(true);
    }

    fn resize_shared_runtime_to_effective_size_before_input(&mut self) {
        self.resize_shared_runtime_to_effective_size_with_pending_agent_resumes(false);
    }

    fn resize_shared_runtime_to_effective_size_with_pending_agent_resumes(
        &mut self,
        start_pending_agent_resumes: bool,
    ) {
        if self.foreground_client_id.is_none() {
            return;
        }
        let Some(client_id) = self.foreground_client_id else {
            return;
        };
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        let (cols, rows) = self.effective_size;
        let area = Rect::new(0, 0, cols, rows);
        if self.app.state.kitty_graphics_enabled && client.cell_size.is_known() {
            crate::ui::compute_view_with_cell_size(
                &mut self.app.state,
                &self.app.terminal_runtimes,
                area,
                client.cell_size,
            );
        } else {
            crate::ui::compute_view_with_runtime_registry(
                &mut self.app.state,
                &self.app.terminal_runtimes,
                area,
            );
        }

        // Shared runtime size changes affect pane wrapping and foreground-driven
        // rendering semantics. Force one fresh frame to every remaining client
        // even if the next rendered buffer compares equal to its cached frame.
        for client in self.clients.values_mut() {
            client.request_full_redraw();
        }
        if !start_pending_agent_resumes {
            self.app.pending_agent_resume_deadline = None;
            return;
        }
        let now = Instant::now();
        self.app.sync_pending_agent_resume_deadline(now);
        if self
            .app
            .start_pending_agent_resumes(self.app.pending_agent_resume_due(now))
        {
            for client in self.clients.values_mut() {
                client.request_full_redraw();
            }
        }
    }

    fn sync_foreground_client_state(&mut self) {
        let Some(client_id) = self.foreground_client_id else {
            self.effective_size = (MIN_COLS, MIN_ROWS);
            self.app.state.outer_terminal_focus = None;
            self.app.state.host_cell_size = crate::kitty_graphics::HostCellSize::default();
            let server_keybindings = self.server_keybindings.clone();
            apply_keybindings(&mut self.app, &server_keybindings);
            self.sync_visible_server_config_diagnostic(false);
            return;
        };
        let Some(client) = self.clients.get(&client_id) else {
            self.foreground_client_id = None;
            self.effective_size = (MIN_COLS, MIN_ROWS);
            self.app.state.outer_terminal_focus = None;
            self.app.state.host_cell_size = crate::kitty_graphics::HostCellSize::default();
            let server_keybindings = self.server_keybindings.clone();
            apply_keybindings(&mut self.app, &server_keybindings);
            self.sync_visible_server_config_diagnostic(false);
            return;
        };

        let terminal_size = client.terminal_size;
        let outer_terminal_focus = client.outer_terminal_focus;
        let host_cell_size = if self.app.state.kitty_graphics_enabled && client.cell_size.is_known()
        {
            client.cell_size
        } else {
            crate::kitty_graphics::HostCellSize::default()
        };
        let host_terminal_theme = client.host_terminal_theme;
        let host_terminal_appearance = client.host_terminal_appearance;
        let host_terminal_appearance_explicit = client.host_terminal_appearance_explicit;
        let uses_local_keybindings = client.keybindings.is_some();
        let keybindings = client
            .keybindings
            .as_deref()
            .unwrap_or(&self.server_keybindings)
            .clone();

        self.effective_size = terminal_size;
        self.app.state.outer_terminal_focus = outer_terminal_focus;
        self.app.state.host_cell_size = host_cell_size;
        apply_keybindings(&mut self.app, &keybindings);
        self.sync_visible_server_config_diagnostic(uses_local_keybindings);
        if outer_terminal_focus == Some(true) {
            self.app.state.mark_active_tab_seen();
        }
        self.app.set_host_terminal_appearance_state(
            host_terminal_appearance,
            host_terminal_appearance_explicit,
        );
        self.app.set_host_terminal_theme(host_terminal_theme);
    }

    #[cfg(unix)]
    fn perform_live_handoff(
        &mut self,
        params: crate::api::schema::ServerLiveHandoffParams,
    ) -> io::Result<()> {
        if !self.managed_runs.is_empty()
            || !self.pending_proofs.is_empty()
            || !self.pending_mission_launches.is_empty()
            || !self.pending_project_launches.is_empty()
        {
            return Err(io::Error::other(
                "live handoff requires managed provider runs and proof checks to stop first",
            ));
        }
        info!("starting live handoff");
        let import_exe = params.import_exe.as_deref().map(std::path::PathBuf::from);
        let socket_path = crate::server::handoff::handoff_socket_path();
        let token = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let listener = match crate::server::handoff::bind_listener(&socket_path) {
            Ok(listener) => listener,
            Err(err) => {
                self.handoff_in_progress = false;
                return Err(err);
            }
        };

        let mut pane_by_terminal = HashMap::new();
        for ws in &self.app.state.workspaces {
            for tab in &ws.tabs {
                for (pane_id, pane) in &tab.panes {
                    pane_by_terminal.insert(pane.attached_terminal_id.clone(), pane_id.raw());
                }
            }
        }
        if pane_by_terminal.len() > crate::server::handoff::MAX_FDS_PER_HANDOFF {
            let _ = std::fs::remove_file(&socket_path);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "live handoff supports at most {} panes in one update; close panes or restart nagi normally",
                    crate::server::handoff::MAX_FDS_PER_HANDOFF
                ),
            ));
        }

        self.handoff_in_progress = true;
        self.disconnect_all_clients_for_handoff();
        let _ = reject_pending_client_connections(&self.client_listener);

        let mut paused_terminal_ids = Vec::new();
        for terminal_id in pane_by_terminal.keys() {
            if let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) {
                if let Err(err) = runtime.pause_handoff_reader(Duration::from_secs(2)) {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    return Err(err);
                }
                paused_terminal_ids.push(terminal_id.clone());
            }
        }

        let mission_fence = match self.mission_runtime.prepare_handoff() {
            Ok(fence) => fence,
            Err(err) => {
                self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                return Err(io::Error::other(format!(
                    "failed to prepare mission journal handoff: {err}"
                )));
            }
        };

        let snapshot = crate::persist::capture(
            &self.app.state.workspaces,
            &self.app.state.terminals,
            &self.app.terminal_runtimes,
            self.app.state.active,
            self.app.state.selected,
            self.app.state.sidebar_width,
            self.app.state.sidebar_section_split,
            self.app.state.collapsed_space_keys.clone(),
        );

        let mut handoff_entries = Vec::new();
        for (terminal_id, runtime) in self.app.terminal_runtimes.iter() {
            let Some(pane_id) = pane_by_terminal.get(terminal_id).copied() else {
                continue;
            };
            let mut handoff_runtime = runtime.handoff_runtime_state(pane_id);
            let has_agent_session = self
                .app
                .state
                .terminals
                .get(terminal_id)
                .is_some_and(|terminal| terminal.persisted_agent_session.is_some());
            if !has_agent_session {
                handoff_runtime.initial_history_ansi = runtime.handoff_history_ansi();
            }
            handoff_entries.push((terminal_id.clone(), handoff_runtime));
        }

        let panes = handoff_entries
            .iter()
            .map(|(_, runtime)| runtime.clone())
            .collect();
        let manifest = crate::server::handoff::manifest_for(
            snapshot,
            panes,
            mission_fence,
            params.expected_protocol,
            params.expected_version,
        );
        let mut import_child = match crate::server::handoff::spawn_handoff_import(
            import_exe.as_deref(),
            &socket_path,
            &token,
        ) {
            Ok(child) => child,
            Err(err) => {
                self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                return Err(err);
            }
        };
        let child_pid = import_child.id();
        info!(pid = child_pid, socket = %socket_path.display(), "spawned handoff import server");

        let mut fds = Vec::new();
        let duplicate_result = (|| {
            for (terminal_id, _) in &handoff_entries {
                let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) else {
                    continue;
                };
                fds.push(runtime.duplicate_handoff_fd()?);
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(err) = duplicate_result {
            for fd in fds {
                let _ = unsafe { libc::close(fd) };
            }
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(err);
        }

        let mut stream = match crate::server::handoff::accept_and_validate_on(
            listener,
            &socket_path,
            &token,
            &manifest,
        ) {
            Ok(stream) => stream,
            Err(err) => {
                for fd in fds {
                    let _ = unsafe { libc::close(fd) };
                }
                crate::server::handoff::cleanup_failed_import_child(&mut import_child);
                self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                return Err(err);
            }
        };

        let send_result = crate::server::handoff::send_fds_and_wait_restored(&mut stream, &fds);
        for fd in fds {
            let _ = unsafe { libc::close(fd) };
        }
        if let Err(err) = send_result {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(err);
        }

        if let Some(api_server) = &self.api_server {
            let _ = api_server.remove_socket_file_if_owned();
        } else {
            let _ = std::fs::remove_file(crate::api::socket_path());
        }
        let _ = remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity);
        if let Err(err) = crate::server::handoff::wait_ready(&mut stream) {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            if let Err(restore_err) = self.wait_then_restore_public_sockets_after_failed_handoff() {
                return Err(io::Error::other(format!(
                    "handoff replacement server did not become ready: {err}; old server could not restore public sockets: {restore_err}"
                )));
            }
            return Err(io::Error::other(format!(
                "handoff replacement server did not become ready: {err}"
            )));
        }
        if let Err(err) = self.mission_runtime.relinquish_handoff() {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            let restore_result = self.wait_then_restore_public_sockets_after_failed_handoff();
            return match restore_result {
                Ok(()) => Err(io::Error::other(format!(
                    "failed to release mission journal ownership: {err}"
                ))),
                Err(restore_err) => Err(io::Error::other(format!(
                    "failed to release mission journal ownership: {err}; old server could not restore public sockets: {restore_err}"
                ))),
            };
        }
        if let Err(err) = crate::server::handoff::report_committed(&mut stream) {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            if let Err(restore_err) = self.wait_then_restore_public_sockets_after_failed_handoff() {
                return Err(io::Error::other(format!(
                    "handoff replacement server was ready, but commit failed: {err}; old server could not restore public sockets: {restore_err}"
                )));
            }
            return Err(err);
        }

        if let Err(err) = crate::server::handoff::wait_owned_ack(&mut stream) {
            if let Err(terminate_err) =
                crate::server::handoff::terminate_import_child(&mut import_child)
            {
                error!(err = %terminate_err, "handoff successor ownership is uncertain; old server must exit");
                self.preserve_runtimes_for_uncertain_handoff();
                return Err(io::Error::other(format!(
                    "handoff ownership acknowledgement failed: {err}; successor could not be terminated: {terminate_err}"
                )));
            }
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            if self.app.state.should_quit {
                return Err(io::Error::other(format!(
                    "handoff ownership acknowledgement failed: {err}; mission ownership could not be restored"
                )));
            }
            if let Err(restore_err) = self.wait_then_restore_public_sockets_after_failed_handoff() {
                return Err(io::Error::other(format!(
                    "handoff ownership acknowledgement failed: {err}; old server could not restore public sockets: {restore_err}"
                )));
            }
            return Err(io::Error::other(format!(
                "handoff successor did not acknowledge ownership: {err}"
            )));
        }
        for (terminal_id, runtime) in self.app.terminal_runtimes.drain_for_handoff() {
            if !pane_by_terminal.contains_key(&terminal_id) {
                continue;
            }
            debug!(terminal = %terminal_id, "preserving pane runtime for handoff");
            runtime.preserve_for_handoff();
        }
        Ok(())
    }

    fn finish_live_handoff_shutdown(&mut self) {
        self.shutting_down = true;
        self.app.state.should_quit = true;
        self.app.no_session = true;
        info!("live handoff completed; old server exiting");
    }

    #[cfg(not(unix))]
    fn perform_live_handoff(
        &mut self,
        _params: crate::api::schema::ServerLiveHandoffParams,
    ) -> io::Result<()> {
        Err(io::Error::other("live handoff is only supported on Unix"))
    }

    fn sync_visible_server_config_diagnostic(&mut self, uses_local_keybindings: bool) {
        let visible = if uses_local_keybindings {
            &self.server_config_diagnostic_without_keybindings
        } else {
            &self.server_config_diagnostic
        };
        if self.app.state.config_diagnostic == self.server_config_diagnostic
            || self.app.state.config_diagnostic == self.server_config_diagnostic_without_keybindings
        {
            self.app.state.config_diagnostic = visible.clone();
        }
    }

    #[cfg(unix)]
    fn restore_public_sockets_after_failed_handoff(&mut self) -> io::Result<()> {
        let api_tx = self
            .api_tx
            .clone()
            .ok_or_else(|| io::Error::other("cannot restore api socket without api sender"))?;
        let api_server = api::start_server(api_tx, self.app.event_hub.clone())?;

        let client_path = client_socket_path();
        prepare_socket_path(&client_path)?;
        let listener = bind_local_listener(&client_path)?;
        restrict_socket_permissions(&client_path)?;
        let client_socket_identity = socket_file_identity(&client_path)?;
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;

        self.api_server = Some(api_server);
        self.client_listener = listener;
        self.client_socket_path = client_path;
        self.client_socket_identity = client_socket_identity;
        Ok(())
    }

    #[cfg(unix)]
    fn wait_then_restore_public_sockets_after_failed_handoff(&mut self) -> io::Result<()> {
        let timeout = crate::server::handoff::COMMIT_TIMEOUT + Duration::from_secs(2);
        wait_for_old_public_sockets_to_close(timeout)?;
        self.restore_public_sockets_after_failed_handoff()
    }

    #[cfg(unix)]
    fn rollback_handoff_before_commit(
        &mut self,
        socket_path: &Path,
        paused_terminal_ids: &[crate::terminal::TerminalId],
    ) {
        if let Err(err) = self.mission_runtime.abort_handoff() {
            error!(err = %err, "failed to restore mission journal ownership after handoff rollback");
            self.preserve_runtimes_for_uncertain_handoff();
            return;
        }
        for terminal_id in paused_terminal_ids {
            if let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) {
                runtime.set_handoff_reader_paused(false);
            }
        }
        self.handoff_in_progress = false;
        let _ = std::fs::remove_file(socket_path);
    }

    #[cfg(unix)]
    fn preserve_runtimes_for_uncertain_handoff(&mut self) {
        for (terminal_id, runtime) in self.app.terminal_runtimes.drain_for_handoff() {
            warn!(terminal = %terminal_id, "preserving pane process during uncertain handoff shutdown");
            runtime.preserve_for_handoff();
        }
        self.app.no_session = true;
        self.app.state.should_quit = true;
        self.shutting_down = true;
    }

    #[cfg(unix)]
    fn nudge_handoff_panes_on_first_client_attach(&mut self) {
        if !self.pending_handoff_repaint_nudge {
            return;
        }
        self.pending_handoff_repaint_nudge = false;
        self.app
            .terminal_runtimes
            .nudge_child_redraw_after_handoff();
    }

    #[cfg(not(unix))]
    fn nudge_handoff_panes_on_first_client_attach(&mut self) {}

    fn reload_server_config(&mut self, notify_success: bool) -> crate::config::ConfigReloadReport {
        let server_keybindings = self.server_keybindings.clone();
        apply_keybindings(&mut self.app, &server_keybindings);
        let report = self.app.apply_config_from_disk(notify_success);
        self.app.take_config_reloaded_from_disk();
        self.server_keybindings = app_keybindings(&self.app);
        let (server_config_diagnostic, server_config_diagnostic_without_keybindings) =
            server_config_diagnostic_summaries(&report.diagnostics);
        self.server_config_diagnostic = server_config_diagnostic;
        self.server_config_diagnostic_without_keybindings =
            server_config_diagnostic_without_keybindings;
        self.sync_foreground_client_state();
        self.acp_endpoint = configured_acp_endpoint(&crate::config::Config::load().config)
            .unwrap_or_else(|error| {
                warn!(err = %error, "ACP provider configuration is invalid");
                None
            });
        report
    }

    fn spawn_managed_provider(
        &self,
        provider: crate::mission::model::ProviderKind,
    ) -> Result<
        crate::managed_provider::ManagedProviderHandle,
        crate::managed_provider::ManagedProviderError,
    > {
        if provider == crate::mission::model::ProviderKind::Acp {
            let endpoint = self
                .acp_endpoint
                .clone()
                .ok_or(crate::managed_provider::ManagedProviderError::AcpEndpointUnavailable)?;
            crate::managed_provider::ManagedProviderSupervisor::spawn_acp(
                endpoint,
                self.provider_event_tx.clone(),
            )
        } else {
            crate::managed_provider::ManagedProviderSupervisor::spawn(
                provider,
                self.managed_provider_executable.clone(),
                self.provider_event_tx.clone(),
            )
        }
    }

    fn foreground_client_outer_focus(&self) -> Option<bool> {
        let client_id = self.foreground_client_id?;
        self.clients.get(&client_id)?.outer_terminal_focus
    }

    fn active_tab_suppresses_notifications(&self, is_active_tab: bool) -> bool {
        crate::app::actions::active_tab_suppresses_notifications(
            is_active_tab,
            self.foreground_client_outer_focus(),
        )
    }

    fn promote_client_to_foreground(&mut self, client_id: u64) -> bool {
        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        client.last_activity = stamp;

        let changed = self.foreground_client_id != Some(client_id);
        self.foreground_client_id = Some(client_id);
        self.sync_foreground_client_state();
        changed
    }

    fn promote_latest_remaining_client(&mut self) -> bool {
        let next_foreground = latest_app_client(&self.clients);
        let changed = next_foreground != self.foreground_client_id;
        self.foreground_client_id = next_foreground;
        self.sync_foreground_client_state();
        changed
    }

    fn app_client_count(&self) -> usize {
        self.clients
            .values()
            .filter(|client| client.is_full_app_client() && client.writer.is_some())
            .count()
    }

    fn has_app_client(&self) -> bool {
        self.app_client_count() > 0
    }

    fn remove_client(&mut self, client_id: u64) -> bool {
        let was_foreground = self.foreground_client_id == Some(client_id);
        self.send_client_graphics_cleanup(client_id);
        let removed = self.clients.remove(&client_id);
        if let Some(removed) = removed {
            crate::server::clipboard_image::remove_files(removed.staged_clipboard_files);
            if let ClientConnectionMode::TerminalAttach { terminal_id } = removed.mode {
                self.terminal_attach_owners.remove(&terminal_id);
                if let Some(terminal_id) = self.terminal_id_by_string(&terminal_id) {
                    self.app
                        .state
                        .direct_attach_resize_locks
                        .remove(&terminal_id);
                }
            }
        }
        if was_foreground {
            self.promote_latest_remaining_client()
        } else {
            false
        }
    }

    fn client_removal_needs_shared_resize(&self, client_id: u64) -> bool {
        if self.foreground_client_id == Some(client_id) {
            return true;
        }
        matches!(
            self.clients.get(&client_id).map(|client| &client.mode),
            Some(
                ClientConnectionMode::TerminalAttach { .. }
                    | ClientConnectionMode::TerminalObserve { .. }
            )
        ) && self.foreground_client_id.is_some()
    }

    fn remove_client_and_resize_if_needed(&mut self, client_id: u64) {
        let needs_shared_resize = self.client_removal_needs_shared_resize(client_id);
        let foreground_changed = self.remove_client(client_id);
        if needs_shared_resize || foreground_changed {
            self.resize_shared_runtime_to_effective_size();
        }
    }

    fn send_client_graphics_cleanup(&mut self, client_id: u64) {
        let (writer, bytes) = match self.clients.get_mut(&client_id) {
            Some(client) => {
                let bytes = client.graphics_cache.clear_bytes();
                (client.writer.as_ref().cloned(), bytes)
            }
            None => return,
        };
        if bytes.is_empty() {
            return;
        }
        let Some(writer) = writer else {
            return;
        };
        let Ok(serialized) = Self::frame_server_message(&ServerMessage::Graphics { bytes }) else {
            return;
        };
        let _ = writer.control.send(serialized);
    }

    fn send_all_clients_graphics_cleanup(&mut self) {
        let client_ids = self.clients.keys().copied().collect::<Vec<_>>();
        for client_id in client_ids {
            self.send_client_graphics_cleanup(client_id);
        }
    }

    fn update_client_host_theme_from_events(
        &mut self,
        client_id: u64,
        events: &[crate::raw_input::RawInputEvent],
    ) -> bool {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };

        if !client.update_host_theme_from_events(events) {
            return false;
        }

        if self.foreground_client_id == Some(client_id) {
            let mut changed = self.app.set_host_terminal_appearance_state(
                client.host_terminal_appearance,
                client.host_terminal_appearance_explicit,
            );
            changed |= self.app.set_host_terminal_theme(client.host_terminal_theme);
            if changed {
                self.resize_shared_runtime_to_effective_size_before_input();
            }
            changed
        } else {
            false
        }
    }

    fn update_client_outer_focus_from_events(
        &mut self,
        client_id: u64,
        events: &[crate::raw_input::RawInputEvent],
    ) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        let Some(next_focus) = client.update_outer_focus_from_events(events) else {
            return;
        };
        if self.foreground_client_id == Some(client_id) {
            self.app.state.outer_terminal_focus = Some(next_focus);
        }
    }

    /// Accepts pending client connections from the non-blocking listener.
    #[cfg(unix)]
    fn accept_client_connections(&mut self) -> io::Result<()> {
        if self.handoff_in_progress {
            return reject_pending_client_connections(&self.client_listener);
        }
        accept_pending_client_connections(
            &self.client_listener,
            &mut self.next_client_id,
            &self.should_quit,
            &self.server_event_tx,
        )
    }

    /// Windows named-pipe clients can block in connect unless the server has a
    /// pending blocking accept. The dedicated accept thread handles that path.
    #[cfg(windows)]
    fn accept_client_connections(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Drains server events from the dedicated channel.
    ///
    /// Uses the original full-render semantics when pane graphics are dormant.
    fn drain_server_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.server_event_rx.try_recv() {
            changed |= self.handle_server_event(ev);
        }
        changed
    }

    /// Returns the strongest render impact from the drained event batch.
    fn drain_server_events_with_render_impact(&mut self) -> RenderImpact {
        let mut impact = RenderImpact::None;
        while let Ok(ev) = self.server_event_rx.try_recv() {
            impact.merge(self.handle_server_event_with_render_impact(ev));
        }
        impact
    }

    fn terminal_id_by_string(&self, terminal_id: &str) -> Option<crate::terminal::TerminalId> {
        self.app
            .state
            .terminals
            .keys()
            .find(|id| id.to_string() == terminal_id)
            .cloned()
    }

    fn runtime_for_terminal_id_string(
        &self,
        terminal_id: &str,
    ) -> Option<&crate::terminal::TerminalRuntime> {
        let terminal_id = self.terminal_id_by_string(terminal_id)?;
        self.app.terminal_runtimes.get(&terminal_id)
    }

    fn resolve_terminal_target_id_string(&self, target: &str) -> Option<String> {
        if self.terminal_id_by_string(target).is_some() {
            return Some(target.to_owned());
        }
        self.app
            .resolve_terminal_target(target)
            .ok()
            .map(|resolved| resolved.terminal_id)
    }

    fn write_client_clipboard_image(
        &mut self,
        client_id: u64,
        extension: &str,
        data: &[u8],
    ) -> std::io::Result<String> {
        let staged = crate::server::clipboard_image::stage(client_id, extension, data)?;
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.staged_clipboard_files.push(staged.path);
        }
        info!(client_id, bytes = data.len(), path = %staged.paste_text, "staged client clipboard image");
        Ok(staged.paste_text)
    }

    fn paste_client_clipboard_image_path(&mut self, client_id: u64, path: String) -> bool {
        if let Some(ClientConnection {
            mode: ClientConnectionMode::TerminalAttach { terminal_id },
            ..
        }) = self.clients.get(&client_id)
        {
            if let Some(runtime) = self.runtime_for_terminal_id_string(terminal_id) {
                let payload = paste_payload_for_runtime(runtime, &path);
                if let Err(err) = runtime.try_send_bytes(Bytes::from(payload)) {
                    warn!(client_id, terminal_id = %terminal_id, err = %err, "terminal attach clipboard image paste failed");
                }
            }
            return true;
        }

        let foreground_changed = self.promote_client_to_foreground(client_id);
        if foreground_changed {
            self.resize_shared_runtime_to_effective_size_before_input();
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.request_semantic_redraw_after_input();
        }
        self.app.route_client_events(
            vec![crate::raw_input::RawInputEvent::Paste(path)],
            self.foreground_client_id == Some(client_id),
        );
        true
    }

    fn resolve_terminal_session_target(
        &mut self,
        client_id: u64,
        target: &str,
        action: &str,
    ) -> Option<String> {
        if !self.client_is_pending_terminal_mode(client_id) {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(
                        format!(
                            "terminal session {action} failed: connection is not pending terminal session"
                        ),
                    ),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return None;
        }

        let Some(terminal_id) = self.resolve_terminal_target_id_string(target) else {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(format!(
                        "terminal session {action} failed: terminal target {target} not found"
                    )),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return None;
        };

        Some(terminal_id)
    }

    fn observe_terminal_client(&mut self, client_id: u64, target: String) -> bool {
        let Some(terminal_id) = self.resolve_terminal_session_target(client_id, &target, "observe")
        else {
            return false;
        };

        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let (cols, rows) = client.terminal_size;
        client.mode = ClientConnectionMode::TerminalObserve {
            terminal_id: terminal_id.clone(),
        };
        client.pending_terminal_attach = false;
        client.render_state.reset_baseline();
        client.last_activity = stamp;
        let was_foreground = self.foreground_client_id == Some(client_id);
        if was_foreground {
            self.promote_latest_remaining_client();
        }

        info!(client_id, cols, rows, terminal_id = %terminal_id, "terminal observe client connected");
        true
    }

    fn control_terminal_client(&mut self, client_id: u64, target: String, takeover: bool) -> bool {
        let Some(terminal_id) = self.resolve_terminal_session_target(client_id, &target, "control")
        else {
            return false;
        };

        self.attach_terminal_client(client_id, terminal_id, takeover)
    }

    fn handle_terminal_attach_scroll(
        &mut self,
        client_id: u64,
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: u8,
    ) -> bool {
        let Some(ClientConnection {
            mode: ClientConnectionMode::TerminalAttach { terminal_id },
            ..
        }) = self.clients.get(&client_id)
        else {
            return false;
        };
        let Some(runtime) = self.runtime_for_terminal_id_string(terminal_id) else {
            return false;
        };

        if let Err(err) =
            apply_terminal_attach_scroll(runtime, source, direction, lines, column, row, modifiers)
        {
            warn!(client_id, terminal_id = %terminal_id, err = %err, "terminal attach scroll failed");
        }
        true
    }

    fn pane_effective_state(&self, pane_id: crate::layout::PaneId) -> crate::detect::AgentState {
        self.app
            .state
            .workspaces
            .iter()
            .find_map(|ws| {
                ws.tabs.iter().find_map(|tab| {
                    let pane = tab.panes.get(&pane_id)?;
                    self.app
                        .state
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .map(|terminal| terminal.state)
                })
            })
            .unwrap_or(crate::detect::AgentState::Unknown)
    }

    fn pane_effective_agent_label(&self, pane_id: crate::layout::PaneId) -> Option<String> {
        self.app.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                let pane = tab.panes.get(&pane_id)?;
                self.app
                    .state
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .and_then(|terminal| terminal.effective_agent_label())
                    .map(str::to_string)
            })
        })
    }

    fn forward_pane_state_update_notifications_to_clients(
        &mut self,
        update: &crate::app::actions::PaneStateUpdate,
    ) {
        if self.app.state.toast_config.delay_seconds != 0 {
            return;
        }

        let is_active_tab = self
            .app
            .state
            .pane_is_in_active_tab(update.ws_idx, update.pane_id);
        let suppress_active_tab_notifications =
            self.active_tab_suppresses_notifications(is_active_tab);

        if self.app.state.sound.allows(update.known_agent) {
            if let Some(sound) =
                crate::app::actions::notification_sound_for_state_change_with_agent_labels(
                    suppress_active_tab_notifications,
                    update.previous_state,
                    update.state,
                    update.previous_agent_label.as_deref(),
                    update.agent_label.as_deref(),
                )
            {
                self.send_notify_to_foreground_client(
                    protocol::NotifyKind::Sound,
                    sound_notify_message(sound),
                    None,
                );
            }
        }

        if !should_forward_toast_to_clients(self.app.state.toast_config.delivery) {
            return;
        }
        let Some(kind) = crate::app::actions::notification_toast_for_pane_state_update(
            suppress_active_tab_notifications,
            update,
        ) else {
            return;
        };
        let Some(ws) = self.app.state.workspaces.get(update.ws_idx) else {
            return;
        };
        let Some(agent_label) = update.agent_label.as_deref() else {
            return;
        };
        let event_text = match kind {
            crate::app::state::ToastKind::NeedsAttention => "needs attention",
            crate::app::state::ToastKind::Finished => "finished",
            crate::app::state::ToastKind::UpdateInstalled => "updated",
        };
        let workspace_label =
            ws.display_name_from(&self.app.state.terminals, &self.app.terminal_runtimes);
        let context = crate::app::actions::notification_context(
            ws,
            &workspace_label,
            update.ws_idx,
            update.pane_id,
        );
        self.send_notify_to_foreground_client(
            toast_notify_kind(self.app.state.toast_config.delivery)
                .expect("toast forwarding requires a client notification kind"),
            format!("{agent_label} {event_text}"),
            non_empty_body(&context),
        );
    }

    fn forward_agent_notification_delivery(
        &mut self,
        delivery: &crate::app::state::AgentNotificationDelivery,
    ) {
        if let Some(sound) = delivery.sound {
            self.send_notify_to_foreground_client(
                protocol::NotifyKind::Sound,
                sound_notify_message(sound),
                None,
            );
        }

        if should_forward_toast_to_clients(self.app.state.toast_config.delivery) {
            if let Some(toast) = &delivery.client_notification {
                self.send_notify_to_foreground_client(
                    toast_notify_kind(self.app.state.toast_config.delivery)
                        .expect("toast forwarding requires a client notification kind"),
                    &toast.title,
                    non_empty_body(&toast.context),
                );
            }
        }
    }

    fn send_notify_to_foreground_client(
        &mut self,
        kind: protocol::NotifyKind,
        message: impl Into<String>,
        body: Option<String>,
    ) -> bool {
        self.send_to_foreground_client(ServerMessage::Notify {
            kind,
            message: message.into(),
            body,
        })
    }

    fn send_flat_toast_to_foreground_client(
        &mut self,
        kind: protocol::NotifyKind,
        message: impl AsRef<str>,
    ) -> bool {
        let (title, body) = crate::terminal_notify::split_message(message.as_ref());
        self.send_notify_to_foreground_client(kind, title, body.map(str::to_string))
    }

    fn recover_managed_runs(&mut self) {
        use crate::{
            managed_provider::{ProviderCommand, SandboxAccess, StartOrResume},
            mission::{
                claims::ClaimRequestId,
                model::{MissionStatus, ProviderMode},
            },
        };

        let at_millis = crate::server::mission_bridge::now_millis();
        for mission in self.mission_runtime.missions() {
            let Some(run) = mission.run.as_ref() else {
                continue;
            };
            if run.mode != ProviderMode::Managed
                || !matches!(
                    mission.status,
                    MissionStatus::Preparing | MissionStatus::Active
                )
            {
                continue;
            }
            let session_id = run.provider_session_id.clone();
            if mission.status != MissionStatus::Active
                || mission.unresolved_attention_count != 0
                || session_id.is_none()
                || self.managed_runs.contains_key(&run.run_id)
            {
                let _ = self.mission_runtime.transition_run(
                    &mission.mission_id,
                    MissionStatus::Blocked,
                    at_millis,
                );
                continue;
            }
            let recovery_id =
                stable_runtime_id("recovery-claim", &[&mission.mission_id, &run.run_id]);
            let request_id = match ClaimRequestId::new(recovery_id) {
                Ok(request_id) => request_id,
                Err(_) => {
                    let _ = self.mission_runtime.transition_run(
                        &mission.mission_id,
                        MissionStatus::Blocked,
                        at_millis,
                    );
                    continue;
                }
            };
            let outcome = match self
                .mission_runtime
                .recover_managed_run(&mission.mission_id, request_id)
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    warn!(mission_id = %mission.mission_id, err = %error, "managed mission recovery is blocked");
                    let _ = self.mission_runtime.transition_run(
                        &mission.mission_id,
                        MissionStatus::Blocked,
                        at_millis,
                    );
                    continue;
                }
            };
            if run.execute_project_recipe {
                if let Err(error) = provision_project_recipe(
                    Path::new(&run.worktree_path),
                    &mission.mission_id,
                    &run.run_id,
                    false,
                ) {
                    warn!(mission_id = %mission.mission_id, err = %error, "project services could not be adopted during recovery");
                    let _ = self.mission_runtime.release_worktree(&outcome.lease);
                    let _ = self.mission_runtime.transition_run(
                        &mission.mission_id,
                        MissionStatus::Blocked,
                        at_millis,
                    );
                    continue;
                }
            }
            let handle = match self.spawn_managed_provider(run.provider) {
                Ok(handle) => handle,
                Err(error) => {
                    warn!(mission_id = %mission.mission_id, err = %error, "managed provider recovery is unavailable");
                    let _ = self.mission_runtime.release_worktree(&outcome.lease);
                    let _ = self.mission_runtime.transition_run(
                        &mission.mission_id,
                        MissionStatus::Blocked,
                        at_millis,
                    );
                    continue;
                }
            };
            let command = ProviderCommand::StartOrResume(StartOrResume {
                run_id: run.run_id.clone(),
                cwd: PathBuf::from(&run.worktree_path),
                resume_session_id: session_id,
                initial_input: String::new(),
                sandbox: SandboxAccess::ReadOnly,
            });
            if let Err(error) = handle.try_send(command) {
                warn!(mission_id = %mission.mission_id, err = %error, "managed provider recovery command was rejected");
                let _ = self.mission_runtime.release_worktree(&outcome.lease);
                let _ = self.mission_runtime.transition_run(
                    &mission.mission_id,
                    MissionStatus::Blocked,
                    at_millis,
                );
                continue;
            }
            self.managed_runs.insert(
                run.run_id.clone(),
                ManagedRun {
                    mission_id: mission.mission_id,
                    provider: run.provider,
                    recovered: true,
                    execute_declared_checks: run.execute_declared_checks,
                    execute_project_recipe: run.execute_project_recipe,
                    handle,
                    lease: outcome.lease,
                    responses: HashMap::new(),
                    inflight_responses: HashMap::new(),
                },
            );
        }
    }

    fn handle_mission_start_api(
        &mut self,
        request_id: &str,
        params: &crate::api::schema::MissionStartParams,
    ) -> String {
        self.handle_mission_start(request_id, params, false)
    }

    fn handle_mission_start(
        &mut self,
        request_id: &str,
        params: &crate::api::schema::MissionStartParams,
        workspace_write_confirmed: bool,
    ) -> String {
        if !self.mission_runtime.is_available() {
            return encode_mission_error(
                request_id,
                "feature_unavailable",
                "mission features are unavailable on this platform",
            );
        }
        if params.execute_project_recipe && !workspace_write_confirmed {
            return encode_mission_error(
                request_id,
                "interactive_consent_required",
                "project setup and services require confirmation in the local mission cockpit",
            );
        }
        use crate::{
            api::schema::{MissionProvider, MissionProviderMode},
            managed_provider::{ProviderCommand, SandboxAccess, StartOrResume},
            mission::{
                claims::ClaimRequestId,
                model::{ProviderKind, ProviderMode},
                runtime::StartRun,
            },
        };

        let provider = match params.provider {
            MissionProvider::Codex => ProviderKind::Codex,
            MissionProvider::ClaudeCode => ProviderKind::ClaudeCode,
            MissionProvider::OpenCode => ProviderKind::OpenCode,
            MissionProvider::Acp => ProviderKind::Acp,
        };
        if provider == ProviderKind::Acp && !workspace_write_confirmed {
            return encode_mission_error(
                request_id,
                "interactive_consent_required",
                "ACP agents require explicit write-scope confirmation in the local mission cockpit",
            );
        }
        if params.mode != MissionProviderMode::Managed {
            return encode_mission_error(
                request_id,
                "provider_mode_unavailable",
                "mission passthrough start is not available yet",
            );
        }
        let Some(spec) = self.mission_runtime.mission(&params.mission_id) else {
            return encode_mission_error(request_id, "mission_not_found", "mission does not exist");
        };
        let worktree_path = params
            .worktree_path
            .clone()
            .unwrap_or_else(|| spec.repository_path.clone());
        if let Some(active) = self.managed_runs.get(&params.run_id) {
            let requested_worktree = std::fs::canonicalize(&worktree_path).ok();
            let durable_matches = spec.run.as_ref().is_some_and(|run| {
                run.run_id == params.run_id
                    && run.provider == provider
                    && run.mode == ProviderMode::Managed
                    && run.execute_declared_checks == params.execute_declared_checks
                    && run.execute_project_recipe == params.execute_project_recipe
                    && requested_worktree.as_deref() == Some(Path::new(&run.worktree_path))
            });
            if active.mission_id == params.mission_id
                && active.provider == provider
                && durable_matches
            {
                return encode_mission_run_started(request_id, spec);
            }
            return encode_mission_error(
                request_id,
                "run_conflict",
                "mission run id is already active with a different specification",
            );
        }
        let at_millis = crate::server::mission_bridge::now_millis();
        let start = self.mission_runtime.start_run(StartRun {
            mission_id: params.mission_id.clone(),
            run_id: params.run_id.clone(),
            provider,
            mode: ProviderMode::Managed,
            worktree_path: worktree_path.clone(),
            request_id: match ClaimRequestId::new(&params.run_id) {
                Ok(request_id) => request_id,
                Err(error) => {
                    return encode_mission_error(request_id, "invalid_run", &error.to_string())
                }
            },
            execute_declared_checks: params.execute_declared_checks,
            execute_project_recipe: params.execute_project_recipe,
            at_millis,
        });
        let outcome = match start {
            Ok(outcome) => outcome,
            Err(error) => {
                return encode_mission_error(
                    request_id,
                    crate::server::mission_bridge::error_code(&error),
                    &error.to_string(),
                )
            }
        };
        let handle = match self.spawn_managed_provider(provider) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = self.mission_runtime.transition_run(
                    &params.mission_id,
                    crate::mission::model::MissionStatus::Failed,
                    at_millis,
                );
                let _ = self.mission_runtime.release_worktree(&outcome.lease);
                return encode_mission_error(
                    request_id,
                    "provider_start_failed",
                    &error.to_string(),
                );
            }
        };
        let prompt = mission_prompt(&outcome.mission);
        let command = ProviderCommand::StartOrResume(StartOrResume {
            run_id: params.run_id.clone(),
            cwd: PathBuf::from(&worktree_path),
            resume_session_id: outcome
                .mission
                .run
                .as_ref()
                .and_then(|run| run.provider_session_id.clone()),
            initial_input: prompt,
            sandbox: if workspace_write_confirmed {
                SandboxAccess::WorkspaceWriteConfirmed
            } else {
                SandboxAccess::ReadOnly
            },
        });
        if let Err(error) = handle.try_send(command) {
            let _ = self.mission_runtime.transition_run(
                &params.mission_id,
                crate::mission::model::MissionStatus::Failed,
                at_millis,
            );
            let _ = self.mission_runtime.release_worktree(&outcome.lease);
            return encode_mission_error(request_id, "provider_start_failed", &error.to_string());
        }
        self.managed_runs.insert(
            params.run_id.clone(),
            ManagedRun {
                mission_id: params.mission_id.clone(),
                provider,
                recovered: false,
                execute_declared_checks: params.execute_declared_checks,
                execute_project_recipe: params.execute_project_recipe,
                handle,
                lease: outcome.lease,
                responses: HashMap::new(),
                inflight_responses: HashMap::new(),
            },
        );

        encode_mission_run_started(request_id, outcome.mission)
    }

    fn handle_mission_handoff_start_api(
        &mut self,
        request_id: &str,
        params: &crate::api::schema::MissionHandoffStartParams,
    ) -> String {
        self.handle_mission_handoff_start(request_id, params, false)
    }

    fn handle_mission_handoff_start(
        &mut self,
        request_id: &str,
        params: &crate::api::schema::MissionHandoffStartParams,
        workspace_write_confirmed: bool,
    ) -> String {
        use crate::{
            api::schema::MissionProvider,
            managed_provider::{ProviderCommand, SandboxAccess, StartOrResume},
            mission::{
                claims::ClaimRequestId,
                handoff::build_preview,
                model::{MissionStatus, ProviderKind, ProviderMode},
                runtime::ContinueRun,
            },
        };

        if !self.mission_runtime.is_available() {
            return encode_mission_error(
                request_id,
                "feature_unavailable",
                "mission features are unavailable on this platform",
            );
        }
        let provider = match params.to {
            MissionProvider::Codex => ProviderKind::Codex,
            MissionProvider::ClaudeCode => ProviderKind::ClaudeCode,
            MissionProvider::OpenCode => ProviderKind::OpenCode,
            MissionProvider::Acp => ProviderKind::Acp,
        };
        if provider == ProviderKind::Acp && !workspace_write_confirmed {
            return encode_mission_error(
                request_id,
                "interactive_consent_required",
                "ACP agents require explicit write-scope confirmation in the local mission cockpit",
            );
        }
        let Some(mission) = self.mission_runtime.mission(&params.mission_id) else {
            return encode_mission_error(request_id, "mission_not_found", "mission does not exist");
        };
        if mission.run.as_ref().is_some_and(|run| {
            run.provider == provider
                && run.mode == ProviderMode::Managed
                && run.handoff_artifact_sha256.as_deref() == Some(params.artifact_sha256.as_str())
                && self.managed_runs.contains_key(&run.run_id)
        }) {
            return encode_mission_run_started(request_id, mission);
        }
        let Some(source_run) = mission.run.as_ref() else {
            return encode_mission_error(
                request_id,
                "invalid_handoff_state",
                "mission has no source run to hand off",
            );
        };
        if source_run.execute_project_recipe && !workspace_write_confirmed {
            return encode_mission_error(
                request_id,
                "interactive_consent_required",
                "continuing this mission restarts project services and requires confirmation in the local mission cockpit",
            );
        }
        let checks = crate::server::mission_bridge::mission_view(mission.clone()).checks;
        let artifact = match build_preview(
            &mission,
            &self.mission_runtime.attention_items(),
            checks,
            params.to,
            params.generated_at_millis,
        ) {
            Ok(artifact) => artifact,
            Err(error) => {
                let error = crate::mission::runtime::MissionRuntimeError::from(error);
                return encode_mission_error(
                    request_id,
                    crate::server::mission_bridge::error_code(&error),
                    &error.to_string(),
                );
            }
        };
        if artifact.artifact_sha256 != params.artifact_sha256 {
            return encode_mission_error(
                request_id,
                "handoff_artifact_changed",
                "mission or workspace changed after the handoff preview; inspect a fresh preview",
            );
        }

        let source_run_id = source_run.run_id.clone();
        let execute_declared_checks = source_run.execute_declared_checks;
        let execute_project_recipe = source_run.execute_project_recipe;
        self.release_managed_run(&source_run_id);
        if execute_project_recipe {
            if let Err(error) = stop_project_services(&params.mission_id, &source_run_id) {
                return encode_mission_error(request_id, "project_recipe_failed", &error);
            }
        }

        let at_millis = crate::server::mission_bridge::now_millis();
        let outcome = match self.mission_runtime.continue_run(ContinueRun {
            mission_id: params.mission_id.clone(),
            source_run_id,
            run_id: artifact.suggested_run_id.clone(),
            provider,
            mode: ProviderMode::Managed,
            request_id: match ClaimRequestId::new(&artifact.suggested_run_id) {
                Ok(request_id) => request_id,
                Err(error) => {
                    return encode_mission_error(request_id, "invalid_run", &error.to_string());
                }
            },
            handoff_artifact_sha256: artifact.artifact_sha256.clone(),
            at_millis,
        }) {
            Ok(outcome) => outcome,
            Err(error) => {
                return encode_mission_error(
                    request_id,
                    crate::server::mission_bridge::error_code(&error),
                    &error.to_string(),
                );
            }
        };
        let worktree_path = outcome
            .mission
            .run
            .as_ref()
            .map(|run| run.worktree_path.clone())
            .unwrap_or_else(|| artifact.worktree_path.clone());
        if execute_project_recipe {
            if let Err(error) = provision_project_recipe(
                Path::new(&worktree_path),
                &params.mission_id,
                &artifact.suggested_run_id,
                false,
            ) {
                let _ = self.mission_runtime.transition_run(
                    &params.mission_id,
                    MissionStatus::Failed,
                    at_millis,
                );
                let _ = self.mission_runtime.release_worktree(&outcome.lease);
                let _ = stop_project_services(&params.mission_id, &artifact.suggested_run_id);
                return encode_mission_error(request_id, "project_recipe_failed", &error);
            }
        }
        let handle = match self.spawn_managed_provider(provider) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = self.mission_runtime.transition_run(
                    &params.mission_id,
                    MissionStatus::Failed,
                    at_millis,
                );
                let _ = self.mission_runtime.release_worktree(&outcome.lease);
                if execute_project_recipe {
                    let _ = stop_project_services(&params.mission_id, &artifact.suggested_run_id);
                }
                return encode_mission_error(
                    request_id,
                    "provider_start_failed",
                    &error.to_string(),
                );
            }
        };
        let command = ProviderCommand::StartOrResume(StartOrResume {
            run_id: artifact.suggested_run_id.clone(),
            cwd: PathBuf::from(&worktree_path),
            resume_session_id: None,
            initial_input: mission_handoff_prompt(&artifact),
            sandbox: if workspace_write_confirmed {
                SandboxAccess::WorkspaceWriteConfirmed
            } else {
                SandboxAccess::ReadOnly
            },
        });
        if let Err(error) = handle.try_send(command) {
            let _ = self.mission_runtime.transition_run(
                &params.mission_id,
                MissionStatus::Failed,
                at_millis,
            );
            let _ = self.mission_runtime.release_worktree(&outcome.lease);
            if execute_project_recipe {
                let _ = stop_project_services(&params.mission_id, &artifact.suggested_run_id);
            }
            return encode_mission_error(request_id, "provider_start_failed", &error.to_string());
        }
        self.managed_runs.insert(
            artifact.suggested_run_id.clone(),
            ManagedRun {
                mission_id: params.mission_id.clone(),
                provider,
                recovered: false,
                execute_declared_checks,
                execute_project_recipe,
                handle,
                lease: outcome.lease,
                responses: HashMap::new(),
                inflight_responses: HashMap::new(),
            },
        );

        encode_mission_run_started(request_id, outcome.mission)
    }

    fn handle_mission_respond_api(
        &mut self,
        request_id: &str,
        _params: &crate::api::schema::MissionRespondParams,
    ) -> String {
        if !self.mission_runtime.is_available() {
            return encode_mission_error(
                request_id,
                "feature_unavailable",
                "mission features are unavailable on this platform",
            );
        }
        encode_mission_error(
            request_id,
            "interactive_consent_required",
            "provider responses must be confirmed in the interactive mission cockpit",
        )
    }

    fn handle_mission_close_api(
        &mut self,
        request_id: &str,
        target: &crate::api::schema::MissionTarget,
    ) -> String {
        use crate::{
            api::schema::{ResponseResult, SuccessResponse},
            mission::{
                claims::{ClaimRequestId, LeaseOwner},
                executor::ClosureExecutionRequest,
                model::MissionStatus,
            },
        };

        let Some(mission) = self.mission_runtime.mission(&target.mission_id) else {
            return encode_mission_error(request_id, "mission_not_found", "mission does not exist");
        };
        if mission.status == MissionStatus::Archived {
            return serde_json::to_string(&SuccessResponse {
                id: request_id.to_owned(),
                result: ResponseResult::MissionCloseAccepted {
                    mission: crate::server::mission_bridge::mission_view(mission),
                },
            })
            .unwrap_or_else(|_| {
                encode_mission_error(
                    request_id,
                    "serialization_failed",
                    "mission close response serialization failed",
                )
            });
        }
        if mission.status != MissionStatus::ReadyToClose {
            return encode_mission_error(
                request_id,
                "mission_not_ready",
                "mission must have a verified ready proof before close",
            );
        }
        let Some(run) = mission.run.as_ref() else {
            return encode_mission_error(
                request_id,
                "mission_runtime_error",
                "mission has no durable run",
            );
        };
        if self
            .pending_proofs
            .get(&run.run_id)
            .is_some_and(|pending| pending.mission_id == mission.mission_id)
        {
            return serde_json::to_string(&SuccessResponse {
                id: request_id.to_owned(),
                result: ResponseResult::MissionCloseAccepted {
                    mission: crate::server::mission_bridge::mission_view(mission),
                },
            })
            .unwrap_or_else(|_| {
                encode_mission_error(
                    request_id,
                    "serialization_failed",
                    "mission close response serialization failed",
                )
            });
        }
        if self.pending_proofs.contains_key(&run.run_id)
            || self.managed_runs.contains_key(&run.run_id)
        {
            return encode_mission_error(
                request_id,
                "mission_busy",
                "mission run is already active",
            );
        }

        let repository = match std::fs::canonicalize(&mission.repository_path) {
            Ok(path) => path,
            Err(error) => {
                return encode_mission_error(request_id, "invalid_repository", &error.to_string())
            }
        };
        let worktree = match std::fs::canonicalize(&run.worktree_path) {
            Ok(path) => path,
            Err(error) => {
                return encode_mission_error(request_id, "invalid_repository", &error.to_string())
            }
        };
        let claim_id = stable_runtime_id(
            "mission-close-claim",
            &[&mission.mission_id, &run.run_id, request_id],
        );
        let claim_id = match ClaimRequestId::new(claim_id) {
            Ok(id) => id,
            Err(error) => {
                return encode_mission_error(request_id, "invalid_request", &error.to_string())
            }
        };
        let lease = match self.mission_runtime.claim_worktree(
            match LeaseOwner::new(&mission.mission_id, &run.run_id) {
                Ok(owner) => owner,
                Err(error) => {
                    return encode_mission_error(
                        request_id,
                        "mission_runtime_error",
                        &error.to_string(),
                    )
                }
            },
            &repository,
            &worktree,
            claim_id,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                return encode_mission_error(
                    request_id,
                    crate::server::mission_bridge::error_code(&error),
                    &error.to_string(),
                )
            }
        };
        let execution = ClosureExecutionRequest {
            mission_id: mission.mission_id.clone(),
            run_id: run.run_id.clone(),
            repository_path: mission.repository_path.clone(),
            worktree_path: run.worktree_path.clone(),
            base_revision: run.base_revision.clone(),
            declarations: mission.check_declarations.clone(),
        };
        if let Err(error) = self.spawn_proof_worker(
            run.run_id.clone(),
            mission.mission_id.clone(),
            lease,
            execution,
        ) {
            return encode_mission_error(request_id, "proof_worker_failed", &error);
        }

        serde_json::to_string(&SuccessResponse {
            id: request_id.to_owned(),
            result: ResponseResult::MissionCloseAccepted {
                mission: crate::server::mission_bridge::mission_view(mission),
            },
        })
        .unwrap_or_else(|_| {
            encode_mission_error(
                request_id,
                "serialization_failed",
                "mission close response serialization failed",
            )
        })
    }

    fn handle_attention_list_api(
        &self,
        request_id: &str,
        params: &crate::api::schema::AttentionListParams,
    ) -> String {
        use crate::api::schema::{AttentionStateV1, ResponseResult, SuccessResponse};

        let items = self
            .app
            .state
            .attention_items
            .iter()
            .filter(|item| {
                params
                    .mission_id
                    .as_ref()
                    .is_none_or(|mission_id| item.mission_id == *mission_id)
            })
            .filter(|item| {
                params.include_closed
                    || matches!(
                        item.state,
                        AttentionStateV1::Open
                            | AttentionStateV1::PendingResponse { .. }
                            | AttentionStateV1::ReconciliationRequired { .. }
                    )
            })
            .cloned()
            .collect();
        serde_json::to_string(&SuccessResponse {
            id: request_id.to_owned(),
            result: ResponseResult::AttentionList { items },
        })
        .unwrap_or_else(|_| {
            encode_mission_error(
                request_id,
                "serialization_failed",
                "attention response serialization failed",
            )
        })
    }

    fn handle_attention_get_api(
        &self,
        request_id: &str,
        target: &crate::api::schema::AttentionTarget,
    ) -> String {
        use crate::api::schema::{ResponseResult, SuccessResponse};

        let Some(item) = self
            .app
            .state
            .attention_items
            .iter()
            .find(|item| item.attention_id == target.attention_id)
            .cloned()
        else {
            return encode_mission_error(
                request_id,
                "attention_not_found",
                "attention item not found",
            );
        };
        serde_json::to_string(&SuccessResponse {
            id: request_id.to_owned(),
            result: ResponseResult::AttentionInfo { item },
        })
        .unwrap_or_else(|_| {
            encode_mission_error(
                request_id,
                "serialization_failed",
                "attention response serialization failed",
            )
        })
    }

    fn handle_mission_respond_authorized(
        &mut self,
        request_id: &str,
        params: &crate::api::schema::MissionRespondParams,
    ) -> String {
        if !self.mission_runtime.is_available() {
            return encode_mission_error(
                request_id,
                "feature_unavailable",
                "mission features are unavailable on this platform",
            );
        }
        use crate::{
            api::schema::{MissionResponseDecision, ResponseResult, SuccessResponse},
            managed_provider::{
                AttentionClass, ManagedProviderError, ProviderCommand, ProviderResponse,
            },
            mission::{
                attention::{AttentionDecision, ResponseFailureDisposition},
                store::{
                    PersistableMissionEvent, PersistedResponseRoute, ResponseAttemptKey,
                    ResponseFailureCode,
                },
            },
        };

        let Some(run) = self.managed_runs.get(&params.run_id) else {
            return encode_mission_error(request_id, "run_not_active", "mission run is not active");
        };
        if run.mission_id != params.mission_id {
            return encode_mission_error(
                request_id,
                "run_mismatch",
                "mission run belongs to another mission",
            );
        }
        let Some(available) = run.responses.get(&params.attention_id).cloned() else {
            return encode_mission_error(
                request_id,
                "attention_not_respondable",
                "attention item has no live managed response channel",
            );
        };
        let handle = run.handle.clone();
        let provider = run.provider;

        let mapped = match (&available.class, params.decision) {
            (
                AttentionClass::CommandApproval
                | AttentionClass::FileChangeApproval
                | AttentionClass::PermissionApproval,
                MissionResponseDecision::ApproveOnce,
            ) if params.answers.is_empty() => {
                (AttentionDecision::ApproveOnce, ProviderResponse::Approve)
            }
            (
                AttentionClass::CommandApproval | AttentionClass::FileChangeApproval,
                MissionResponseDecision::ApproveForSession,
            ) if params.answers.is_empty() => (
                AttentionDecision::ApproveForSession,
                ProviderResponse::ApproveForSession,
            ),
            (
                AttentionClass::CommandApproval
                | AttentionClass::FileChangeApproval
                | AttentionClass::PermissionApproval,
                MissionResponseDecision::Deny,
            ) if params.answers.is_empty() => (AttentionDecision::Deny, ProviderResponse::Decline),
            (AttentionClass::UserInput, MissionResponseDecision::Answer)
                if validate_managed_answers(&params.answers, &available.questions).is_ok() =>
            {
                (
                    AttentionDecision::Answer,
                    ProviderResponse::Answers(params.answers.clone()),
                )
            }
            _ => {
                return encode_mission_error(
                    request_id,
                    "invalid_response",
                    "decision or answers do not match the provider request",
                )
            }
        };

        let request_generation = 1;
        let attempt = match self.mission_runtime.next_response_attempt(
            &params.mission_id,
            &params.attention_id,
            request_generation,
        ) {
            Ok(attempt) => attempt,
            Err(error) => {
                return encode_mission_error(
                    request_id,
                    crate::server::mission_bridge::error_code(&error),
                    &error.to_string(),
                )
            }
        };
        let key = match ResponseAttemptKey::new(&params.attention_id, request_generation, attempt) {
            Ok(key) => key,
            Err(error) => {
                return encode_mission_error(request_id, "invalid_response", &error.to_string())
            }
        };
        let provider_request_id = available.token.request_id().to_owned();
        let at_millis = crate::server::mission_bridge::now_millis();
        let event_id = stable_runtime_id(
            "response-request",
            &[
                &params.mission_id,
                &params.attention_id,
                &request_generation.to_string(),
                &attempt.to_string(),
            ],
        );
        if let Err(error) = self.mission_runtime.commit(
            &event_id,
            PersistableMissionEvent::ResponseRequested {
                mission_id: params.mission_id.clone(),
                key: key.clone(),
                route: PersistedResponseRoute::managed(
                    provider,
                    &params.run_id,
                    available.session_id,
                    &provider_request_id,
                ),
                decision: mapped.0,
                actor_id: "local-user".to_owned(),
                at_millis,
            },
        ) {
            return encode_mission_error(
                request_id,
                crate::server::mission_bridge::error_code(&error),
                &error.to_string(),
            );
        }

        if let Err(error) = handle.try_send(ProviderCommand::Respond {
            token: available.token,
            response: mapped.1,
        }) {
            let failure_id = stable_runtime_id(
                "response-not-sent",
                &[
                    &params.mission_id,
                    &params.attention_id,
                    &attempt.to_string(),
                ],
            );
            let rollback = self.mission_runtime.commit(
                &failure_id,
                PersistableMissionEvent::ResponseFailed {
                    mission_id: params.mission_id.clone(),
                    key,
                    disposition: ResponseFailureDisposition::DefinitelyNotApplied,
                    code: ResponseFailureCode::DisconnectedBeforeWrite,
                    at_millis,
                },
            );
            if rollback.is_err() {
                let _ = self.mission_runtime.transition_run(
                    &params.mission_id,
                    crate::mission::model::MissionStatus::Failed,
                    at_millis,
                );
                self.release_managed_run(&params.run_id);
                return encode_mission_error(
                    request_id,
                    "response_state_uncertain",
                    "provider response was not sent but its durable state could not be reconciled",
                );
            }
            if matches!(error, ManagedProviderError::Disconnected) {
                let _ = self.mission_runtime.transition_run(
                    &params.mission_id,
                    crate::mission::model::MissionStatus::Failed,
                    at_millis,
                );
                self.release_managed_run(&params.run_id);
            }
            return encode_mission_error(request_id, "provider_unavailable", &error.to_string());
        }

        if let Some(run) = self.managed_runs.get_mut(&params.run_id) {
            run.responses.remove(&params.attention_id);
            run.inflight_responses.insert(
                provider_request_id,
                PendingManagedResponse {
                    attention_id: params.attention_id.clone(),
                    key,
                },
            );
        }
        serde_json::to_string(&SuccessResponse {
            id: request_id.to_owned(),
            result: ResponseResult::MissionResponseQueued {
                mission_id: params.mission_id.clone(),
                attention_id: params.attention_id.clone(),
            },
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"serialization_failed","message":"mission response serialization failed"}}"#
                .to_owned()
        })
    }

    fn handle_provider_event(&mut self, event: crate::managed_provider::ProviderEvent) -> bool {
        use crate::{
            managed_provider::{AttentionClass, ProviderEvent, TurnOutcome},
            mission::{
                attention::AttentionRisk,
                model::MissionStatus,
                store::{PersistableMissionEvent, PersistedAttentionState},
            },
        };

        let at_millis = crate::server::mission_bridge::now_millis();
        match event {
            ProviderEvent::Ready { run_id, session_id } => {
                let Some(mission_id) = self
                    .managed_runs
                    .get(&run_id)
                    .map(|run| run.mission_id.clone())
                else {
                    return false;
                };
                let durable = self.mission_runtime.mission(&mission_id);
                let ready = durable.as_ref().is_some_and(|mission| {
                    mission.status == MissionStatus::Active
                        && mission.run.as_ref().is_some_and(|run| {
                            run.run_id == run_id
                                && run.provider_session_id.as_deref() == Some(session_id.as_str())
                        })
                });
                let result = if ready {
                    Ok(())
                } else {
                    self.mission_runtime
                        .bind_provider_session(&mission_id, &run_id, &session_id, at_millis)
                        .map(|_| ())
                };
                match result {
                    Ok(_) => true,
                    Err(_) => {
                        let _ = self.mission_runtime.transition_run(
                            &mission_id,
                            MissionStatus::Failed,
                            at_millis,
                        );
                        self.release_managed_run(&run_id);
                        true
                    }
                }
            }
            ProviderEvent::Working { .. } | ProviderEvent::OutputDelta { .. } => false,
            ProviderEvent::AttentionRequested { run_id, attention } => {
                let Some(run) = self.managed_runs.get_mut(&run_id) else {
                    return false;
                };
                let attention_id =
                    stable_runtime_id("attention", &[&run_id, attention.token.request_id()]);
                let risk = match attention.class {
                    AttentionClass::UserInput => AttentionRisk::Low,
                    AttentionClass::CommandApproval | AttentionClass::FileChangeApproval => {
                        AttentionRisk::High
                    }
                    AttentionClass::PermissionApproval => AttentionRisk::Critical,
                };
                let event_id = stable_runtime_id("attention-open", &[&run_id, &attention_id]);
                let committed = self
                    .mission_runtime
                    .commit(
                        &event_id,
                        PersistableMissionEvent::AttentionChanged {
                            mission_id: run.mission_id.clone(),
                            attention_id: attention_id.clone(),
                            state: PersistedAttentionState::Open,
                            risk,
                            at_millis,
                        },
                    )
                    .is_ok();
                if committed {
                    run.responses.insert(
                        attention_id,
                        AvailableManagedResponse {
                            token: attention.token,
                            class: attention.class,
                            session_id: attention.thread_id,
                            requested_action: attention.requested_action,
                            questions: attention.questions,
                            created_at_millis: at_millis,
                        },
                    );
                }
                committed
            }
            ProviderEvent::ResponseResolved { run_id, request_id } => {
                let pending = self
                    .managed_runs
                    .get(&run_id)
                    .and_then(|run| run.inflight_responses.get(&request_id).cloned());
                let Some(pending) = pending else {
                    return false;
                };
                let Some(mission_id) = self
                    .managed_runs
                    .get(&run_id)
                    .map(|run| run.mission_id.clone())
                else {
                    return false;
                };
                let event_id = stable_runtime_id(
                    "response-ack",
                    &[&mission_id, &pending.attention_id, &request_id],
                );
                let committed = self
                    .mission_runtime
                    .commit(
                        &event_id,
                        PersistableMissionEvent::ResponseAcknowledged {
                            mission_id,
                            key: pending.key,
                            acknowledgement_hash: None,
                            at_millis,
                        },
                    )
                    .is_ok();
                if committed {
                    if let Some(run) = self.managed_runs.get_mut(&run_id) {
                        run.inflight_responses.remove(&request_id);
                    }
                }
                committed
            }
            ProviderEvent::TurnCompleted {
                run_id, outcome, ..
            } => {
                let Some((mission_id, execute_declared_checks)) = self
                    .managed_runs
                    .get(&run_id)
                    .map(|run| (run.mission_id.clone(), run.execute_declared_checks))
                else {
                    return false;
                };
                let status = match outcome {
                    TurnOutcome::Completed => MissionStatus::ReviewRequired,
                    TurnOutcome::Interrupted => MissionStatus::Blocked,
                    TurnOutcome::Failed => MissionStatus::Failed,
                };
                let changed = self
                    .mission_runtime
                    .transition_run(&mission_id, status, at_millis)
                    .is_ok();
                if outcome == TurnOutcome::Failed {
                    self.mark_inflight_delivery_unknown(
                        &run_id,
                        crate::mission::store::ResponseFailureCode::TransportClosed,
                        at_millis,
                    );
                }
                if outcome == TurnOutcome::Completed && changed && execute_declared_checks {
                    if !self.start_closure_execution(&run_id) {
                        self.release_managed_run(&run_id);
                    }
                } else {
                    self.release_managed_run(&run_id);
                }
                changed
            }
            ProviderEvent::TransportFailed { run_id, .. } | ProviderEvent::Stopped { run_id } => {
                let run_state = self
                    .managed_runs
                    .get(&run_id)
                    .map(|run| (run.mission_id.clone(), run.recovered));
                let changed = run_state.is_some_and(|(mission_id, recovered)| {
                    let status = if recovered {
                        MissionStatus::Blocked
                    } else {
                        MissionStatus::Failed
                    };
                    self.mission_runtime
                        .transition_run(&mission_id, status, at_millis)
                        .is_ok()
                });
                self.mark_inflight_delivery_unknown(
                    &run_id,
                    crate::mission::store::ResponseFailureCode::TransportClosed,
                    at_millis,
                );
                self.release_managed_run(&run_id);
                changed
            }
        }
    }

    fn start_closure_execution(&mut self, run_id: &str) -> bool {
        use crate::mission::executor::ClosureExecutionRequest;

        let Some(run) = self.managed_runs.remove(run_id) else {
            return false;
        };
        let _ = run
            .handle
            .try_send(crate::managed_provider::ProviderCommand::Shutdown);
        let Some(mission) = self.mission_runtime.mission(&run.mission_id) else {
            let _ = self.mission_runtime.release_worktree(&run.lease);
            return false;
        };
        let Some(durable_run) = mission.run.as_ref() else {
            let _ = self.mission_runtime.release_worktree(&run.lease);
            return false;
        };
        let request = ClosureExecutionRequest {
            mission_id: mission.mission_id.clone(),
            run_id: durable_run.run_id.clone(),
            repository_path: mission.repository_path.clone(),
            worktree_path: durable_run.worktree_path.clone(),
            base_revision: durable_run.base_revision.clone(),
            declarations: mission.check_declarations.clone(),
        };
        self.spawn_proof_worker(run_id.to_owned(), mission.mission_id, run.lease, request)
            .is_ok()
    }

    fn spawn_proof_worker(
        &mut self,
        run_id: String,
        mission_id: String,
        lease: crate::mission::claims::WorktreeLease,
        request: crate::mission::executor::ClosureExecutionRequest,
    ) -> Result<(), String> {
        use crate::mission::executor::execute_closure_cancellable;

        if self.pending_proofs.contains_key(&run_id) {
            let _ = self.mission_runtime.release_worktree(&lease);
            return Err("a proof worker is already running for this mission run".into());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.pending_proofs.insert(
            run_id.clone(),
            PendingProofRun {
                mission_id,
                lease,
                cancelled: cancelled.clone(),
            },
        );
        let sender = self.proof_event_tx.clone();
        let worker_run_id = run_id.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("nagi-proof-{run_id}"))
            .spawn(move || {
                let result = execute_closure_cancellable(request, &cancelled)
                    .map_err(|error| error.to_string());
                let _ = sender.blocking_send(ProofWorkerEvent {
                    run_id: worker_run_id,
                    result,
                });
            });
        if let Err(error) = spawn {
            warn!(run_id, err = %error, "mission proof worker could not start");
            if let Some(pending) = self.pending_proofs.remove(&run_id) {
                pending.cancelled.store(true, Ordering::Release);
                let _ = self.mission_runtime.release_worktree(&pending.lease);
            }
            return Err(error.to_string());
        }
        Ok(())
    }

    fn handle_proof_event(&mut self, event: ProofWorkerEvent) -> bool {
        let Some(pending) = self.pending_proofs.remove(&event.run_id) else {
            return false;
        };
        let result = match event.result {
            Ok(pack) => self.mission_runtime.finalize_evidence_pack(
                pack,
                &pending.lease,
                crate::server::mission_bridge::now_millis(),
            ),
            Err(error) => {
                warn!(
                    mission_id = %pending.mission_id,
                    run_id = %event.run_id,
                    err = %error,
                    "mission closure checks did not produce a proof pack"
                );
                let _ = self.mission_runtime.release_worktree(&pending.lease);
                return true;
            }
        };
        match result {
            Ok(outcome) => {
                info!(
                    mission_id = %outcome.mission.mission_id,
                    pack_digest = %outcome.pack_digest,
                    verified = outcome.verified,
                    "mission proof pack finalized"
                );
                if outcome.mission.status == crate::mission::model::MissionStatus::Archived {
                    if let Some(run) = outcome
                        .mission
                        .run
                        .as_ref()
                        .filter(|run| run.execute_project_recipe)
                    {
                        let _ = stop_project_services(&outcome.mission.mission_id, &run.run_id);
                        let worktree = PathBuf::from(&run.worktree_path);
                        std::thread::spawn(move || run_project_cleanup(worktree));
                    }
                }
            }
            Err(error) => {
                warn!(
                    mission_id = %pending.mission_id,
                    run_id = %event.run_id,
                    err = %error,
                    "mission proof pack could not be finalized"
                );
            }
        }
        let _ = self.mission_runtime.release_worktree(&pending.lease);
        true
    }

    fn mark_inflight_delivery_unknown(
        &mut self,
        run_id: &str,
        code: crate::mission::store::ResponseFailureCode,
        at_millis: u64,
    ) {
        use crate::mission::store::{PersistableMissionEvent, ResponseFailureDisposition};

        let Some(run) = self.managed_runs.get(run_id) else {
            return;
        };
        let mission_id = run.mission_id.clone();
        let pending = run
            .inflight_responses
            .iter()
            .map(|(request_id, response)| (request_id.clone(), response.clone()))
            .collect::<Vec<_>>();
        for (request_id, response) in pending {
            let event_id = stable_runtime_id(
                "response-unknown",
                &[&mission_id, &response.attention_id, run_id, &request_id],
            );
            let _ = self.mission_runtime.commit(
                &event_id,
                PersistableMissionEvent::ResponseFailed {
                    mission_id: mission_id.clone(),
                    key: response.key,
                    disposition: ResponseFailureDisposition::DeliveryUnknown,
                    code,
                    at_millis,
                },
            );
        }
    }

    fn release_managed_run(&mut self, run_id: &str) {
        if let Some(run) = self.managed_runs.remove(run_id) {
            let _ = run
                .handle
                .try_send(crate::managed_provider::ProviderCommand::Shutdown);
            let _ = self.mission_runtime.release_worktree(&run.lease);
            if run.execute_project_recipe
                && self
                    .mission_runtime
                    .mission(&run.mission_id)
                    .is_some_and(|mission| {
                        mission.status == crate::mission::model::MissionStatus::Failed
                    })
            {
                let _ = stop_project_services(&run.mission_id, run_id);
            }
        }
    }

    fn handle_notification_show_api(
        &mut self,
        id: String,
        params: api::schema::NotificationShowParams,
    ) -> String {
        use api::schema::{NotificationShowReason, ResponseResult};

        let Some(title) = sanitize_notification_text(&params.title, 80) else {
            return serde_json::to_string(&api::schema::ErrorResponse {
                id,
                error: api::schema::ErrorBody {
                    code: "invalid_params".into(),
                    message: "notification title is empty".into(),
                },
            })
            .unwrap_or_else(|_| "{}".to_string());
        };

        match self.app.state.toast_config.delivery {
            config::ToastDelivery::Off => {
                return serde_json::to_string(&api::schema::SuccessResponse {
                    id,
                    result: ResponseResult::NotificationShow {
                        shown: false,
                        reason: NotificationShowReason::Disabled,
                    },
                })
                .unwrap_or_else(|_| "{}".to_string());
            }
            config::ToastDelivery::Nagi => {
                let sound = params.sound;
                let response = self.app.handle_api_request_after_internal_events_drained(
                    api::schema::Request {
                        id,
                        method: api::schema::Method::NotificationShow(params),
                    },
                );
                if notification_show_response_shown(&response) {
                    self.forward_api_notification_sound(sound);
                }
                return response;
            }
            config::ToastDelivery::Terminal | config::ToastDelivery::System => {}
        }

        let body = params
            .body
            .as_deref()
            .and_then(|body| sanitize_notification_text(body, 240));
        if self.app.api_notification_rate_limited(Instant::now()) {
            return serde_json::to_string(&api::schema::SuccessResponse {
                id,
                result: ResponseResult::NotificationShow {
                    shown: false,
                    reason: NotificationShowReason::RateLimited,
                },
            })
            .unwrap_or_else(|_| "{}".to_string());
        }
        let kind = toast_notify_kind(self.app.state.toast_config.delivery)
            .expect("terminal/system delivery has notify kind");
        let shown = self.send_notify_to_foreground_client(kind, title, body);
        if shown {
            self.app.mark_api_notification_shown(Instant::now());
            self.forward_api_notification_sound(params.sound);
        }
        let reason = if shown {
            NotificationShowReason::Shown
        } else {
            NotificationShowReason::NoForegroundClient
        };

        serde_json::to_string(&api::schema::SuccessResponse {
            id,
            result: ResponseResult::NotificationShow { shown, reason },
        })
        .unwrap_or_else(|_| "{}".to_string())
    }

    fn handle_client_window_title_api(&mut self, id: String, title: Option<String>) -> String {
        use api::schema::{ClientWindowTitleReason, ResponseResult};

        let title = match title {
            Some(title) => match sanitize_window_title_text(&title, 200) {
                Some(title) => Some(title),
                None => {
                    return serde_json::to_string(&api::schema::ErrorResponse {
                        id,
                        error: api::schema::ErrorBody {
                            code: "invalid_params".into(),
                            message: "window title is empty".into(),
                        },
                    })
                    .unwrap_or_else(|_| "{}".to_string());
                }
            },
            None => None,
        };
        let set_title = title.is_some();
        let changed = self.send_to_foreground_client(ServerMessage::WindowTitle { title });
        let reason = match (changed, set_title) {
            (true, true) => ClientWindowTitleReason::Set,
            (true, false) => ClientWindowTitleReason::Cleared,
            (false, _) => ClientWindowTitleReason::NoForegroundClient,
        };
        serde_json::to_string(&api::schema::SuccessResponse {
            id,
            result: ResponseResult::ClientWindowTitle { changed, reason },
        })
        .unwrap_or_else(|_| "{}".to_string())
    }

    fn forward_api_notification_sound(&mut self, sound: api::schema::NotificationShowSound) {
        let Some(sound) = sound.to_sound() else {
            return;
        };
        self.send_notify_to_foreground_client(
            protocol::NotifyKind::Sound,
            sound_notify_message(sound),
            None,
        );
    }

    /// Handles a single internal event with forwarding logic for clipboard,
    /// sound, and toast notifications to connected clients.
    ///
    /// ALL internal events MUST be routed through this method to ensure
    /// clipboard/notify forwarding is never bypassed. Do not call
    /// `self.app.handle_internal_event()` directly for any internal event
    /// in the headless server — use this method instead.
    ///
    /// Returns true if the event changed visual state (requiring a re-render).
    fn handle_internal_event_with_forwarding(&mut self, ev: AppEvent) -> bool {
        match &ev {
            AppEvent::ClipboardWrite { content } => {
                // Clipboard writes are client-local side effects. Forward them only to
                // the foreground client instead of broadcasting to every attached client.
                let data = base64::engine::general_purpose::STANDARD.encode(content.as_slice());
                if self.send_to_foreground_client(ServerMessage::Clipboard { data }) {
                    self.app.show_clipboard_feedback();
                }
                true
            }
            AppEvent::PrefixInputSource { active } => {
                // Input-source switching is a client-local host side effect; forward it to the
                // foreground client (which owns the real TIS switch + run-loop pump), like clipboard.
                self.send_to_foreground_client(ServerMessage::PrefixInputSource {
                    active: *active,
                });
                true
            }
            AppEvent::StateChanged { pane_id, agent, .. } => {
                // Capture toast before handling.
                let toast_before = self.app.state.toast.clone();
                let pane_id_val = *pane_id;
                let agent_val = *agent;

                // Find the previous effective state of this pane before the event
                // is processed. Notifications must follow effective state changes,
                // not raw fallback reports that may be masked by hook authority.
                let prev_state = self.pane_effective_state(pane_id_val);
                let prev_agent_label = self.pane_effective_agent_label(pane_id_val);

                // Handle the state change (updates pane state, sets toast on AppState).
                // Headless mode disables local sound playback separately from the
                // sound policy so reloads can keep server-side notification policy live.
                self.sync_foreground_client_state();
                self.app.handle_internal_event(ev);

                // Forward sound notification to clients when server-side sound policy allows it.
                let is_active_tab = self
                    .app
                    .state
                    .active
                    .and_then(|ws_idx| self.app.state.workspaces.get(ws_idx))
                    .is_some_and(|ws| {
                        ws.find_tab_index_for_pane(pane_id_val)
                            .is_some_and(|tab_idx| ws.active_tab_index() == tab_idx)
                    });

                let suppress_active_tab_notifications =
                    self.active_tab_suppresses_notifications(is_active_tab);

                let next_state = self.pane_effective_state(pane_id_val);
                let next_agent_label = self.pane_effective_agent_label(pane_id_val);

                if self.app.state.toast_config.delay_seconds == 0
                    && self.app.state.sound.allows(agent_val)
                {
                    if let Some(sound) =
                        crate::app::actions::notification_sound_for_state_change_with_agent_labels(
                            suppress_active_tab_notifications,
                            prev_state,
                            next_state,
                            prev_agent_label.as_deref(),
                            next_agent_label.as_deref(),
                        )
                    {
                        self.send_notify_to_foreground_client(
                            protocol::NotifyKind::Sound,
                            sound_notify_message(sound),
                            None,
                        );
                    }
                }

                let toast_msg = if self.app.state.toast_config.delay_seconds == 0
                    && should_forward_toast_to_clients(self.app.state.toast_config.delivery)
                {
                    if self.app.state.toast.is_some() && self.app.state.toast != toast_before {
                        self.app
                            .state
                            .toast
                            .as_ref()
                            .map(|toast| format!("{}: {}", toast.title, toast.context))
                    } else {
                        toast_message_from_state_change(
                            &self.app.state,
                            &self.app.terminal_runtimes,
                            pane_id_val,
                            suppress_active_tab_notifications,
                            prev_state,
                            next_state,
                            prev_agent_label.as_deref(),
                        )
                    }
                } else {
                    None
                };

                if let Some(msg) = toast_msg {
                    self.send_flat_toast_to_foreground_client(
                        toast_notify_kind(self.app.state.toast_config.delivery)
                            .expect("toast forwarding requires a client notification kind"),
                        msg,
                    );
                }

                true
            }
            AppEvent::HookStateReported {
                pane_id,
                agent_label,
                ..
            } => {
                // Hook reports can be stale or no-op after sequence rejection.
                // Forward only effective state changes observed after handling.
                let toast_before = self.app.state.toast.clone();
                let pane_id_val = *pane_id;
                let agent_val = crate::detect::parse_agent_label(agent_label);

                // Capture the previous effective state for this pane. Hook reports
                // are already folded into pane.state; raw hook transitions must not
                // produce a second notification path.
                let prev_state = self.pane_effective_state(pane_id_val);
                let prev_agent_label = self.pane_effective_agent_label(pane_id_val);

                self.sync_foreground_client_state();
                self.app.handle_internal_event(ev);

                // Forward sound notification based on the effective transition when
                // server-side sound policy allows it.
                let is_active_tab = self
                    .app
                    .state
                    .active
                    .and_then(|ws_idx| self.app.state.workspaces.get(ws_idx))
                    .is_some_and(|ws| {
                        ws.find_tab_index_for_pane(pane_id_val)
                            .is_some_and(|tab_idx| ws.active_tab_index() == tab_idx)
                    });

                let suppress_active_tab_notifications =
                    self.active_tab_suppresses_notifications(is_active_tab);

                let next_state = self.pane_effective_state(pane_id_val);
                let next_agent_label = self.pane_effective_agent_label(pane_id_val);

                if self.app.state.toast_config.delay_seconds == 0
                    && self.app.state.sound.allows(agent_val)
                {
                    if let Some(sound) =
                        crate::app::actions::notification_sound_for_state_change_with_agent_labels(
                            suppress_active_tab_notifications,
                            prev_state,
                            next_state,
                            prev_agent_label.as_deref(),
                            next_agent_label.as_deref(),
                        )
                    {
                        self.send_notify_to_foreground_client(
                            protocol::NotifyKind::Sound,
                            sound_notify_message(sound),
                            None,
                        );
                    }
                }

                let toast_msg = if self.app.state.toast_config.delay_seconds == 0
                    && should_forward_toast_to_clients(self.app.state.toast_config.delivery)
                {
                    if self.app.state.toast.is_some() && self.app.state.toast != toast_before {
                        self.app
                            .state
                            .toast
                            .as_ref()
                            .map(|toast| format!("{}: {}", toast.title, toast.context))
                    } else {
                        toast_message_from_state_change(
                            &self.app.state,
                            &self.app.terminal_runtimes,
                            pane_id_val,
                            suppress_active_tab_notifications,
                            prev_state,
                            next_state,
                            prev_agent_label.as_deref(),
                        )
                    }
                } else {
                    None
                };

                if let Some(msg) = toast_msg {
                    self.send_flat_toast_to_foreground_client(
                        toast_notify_kind(self.app.state.toast_config.delivery)
                            .expect("toast forwarding requires a client notification kind"),
                        msg,
                    );
                }

                true
            }
            AppEvent::UpdateReady {
                version,
                install_command,
            } => {
                let toast_before = self.app.state.toast.clone();
                let version = version.clone();
                let install_command = install_command.clone();

                self.app.handle_internal_event(ev);

                let toast_msg =
                    if should_forward_toast_to_clients(self.app.state.toast_config.delivery) {
                        if self.app.state.toast.is_some() && self.app.state.toast != toast_before {
                            self.app
                                .state
                                .toast
                                .as_ref()
                                .map(|toast| format!("{}: {}", toast.title, toast.context))
                        } else {
                            Some(format!(
                                "v{version} available: {}",
                                crate::update::update_install_instruction(&install_command)
                            ))
                        }
                    } else {
                        None
                    };

                if let Some(msg) = toast_msg {
                    self.send_flat_toast_to_foreground_client(
                        toast_notify_kind(self.app.state.toast_config.delivery)
                            .expect("toast forwarding requires a client notification kind"),
                        msg,
                    );
                }

                true
            }
            AppEvent::PaneDied { pane_id } => {
                let pane_id_val = *pane_id;
                let terminal_id = self.app.state.workspaces.iter().find_map(|ws| {
                    ws.tabs.iter().find_map(|tab| {
                        tab.panes
                            .get(pane_id)
                            .map(|pane| pane.attached_terminal_id.to_string())
                    })
                });
                if let Some(update) = self
                    .app
                    .state
                    .publish_pane_process_exit_if_agent(pane_id_val)
                {
                    self.app.emit_pane_state_update(&update);
                    self.forward_pane_state_update_notifications_to_clients(&update);
                }

                self.app.handle_internal_event(ev);

                if self.app.find_pane(pane_id_val).is_none() {
                    if let Some(terminal_id) = terminal_id {
                        self.shutdown_terminal_stream_clients(
                            &terminal_id,
                            format!("terminal {terminal_id} exited"),
                        );
                    }
                }

                true
            }
            _ => {
                self.app.handle_internal_event(ev);
                true
            }
        }
    }

    /// Drains internal events, forwarding clipboard, sound, and toast
    /// notifications to connected clients instead of processing them locally.
    ///
    /// In the monolithic mode:
    /// - `ClipboardWrite` events are written to stdout via `write_osc52_bytes`.
    /// - Sound notifications are played locally via `sound::play`.
    /// - Toast notifications are set on AppState and rendered into the frame.
    ///
    /// In the headless server, there is no stdout terminal or audio subsystem,
    /// so we:
    /// - Forward `ClipboardWrite` as `ServerMessage::Clipboard` to the
    ///   foreground client only.
    /// - Detect when a sound would be played and forward as
    ///   `ServerMessage::Notify { kind: Sound }` to the foreground client.
    /// - Detect when a toast is set on AppState and forward as
    ///   `ServerMessage::Notify` to the foreground client for terminal/system delivery.
    fn drain_internal_events_with_forwarding(&mut self) -> bool {
        self.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_DRAIN_LIMIT)
            .1
    }

    fn drain_all_internal_events_with_forwarding(&mut self) -> bool {
        let mut changed = false;
        loop {
            let (had_event, batch_changed) =
                self.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_DRAIN_LIMIT);
            changed |= batch_changed;
            if !had_event {
                break;
            }
        }
        changed
    }

    fn drain_internal_events_with_forwarding_up_to(&mut self, limit: usize) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.app.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_forwarding(ev);
        }
        (had_event, changed)
    }

    fn drain_client_config_reload_request(&mut self) {
        if !self.app.state.request_client_config_reload {
            return;
        }
        self.app.state.request_client_config_reload = false;
        self.send_to_all_clients(ServerMessage::ReloadSoundConfig);
    }

    /// Encodes a server message into a length-prefixed frame.
    fn frame_server_message(msg: &ServerMessage) -> Result<Vec<u8>, protocol::FramingError> {
        Self::frame_server_message_with_max(msg, MAX_FRAME_SIZE)
    }

    /// Encodes a server message using an explicit payload cap.
    fn frame_server_message_with_max(
        msg: &ServerMessage,
        max_frame_size: usize,
    ) -> Result<Vec<u8>, protocol::FramingError> {
        let mut framed = Vec::new();
        protocol::write_message(&mut framed, msg)?;
        let payload_len = framed.len().saturating_sub(4);
        if payload_len > max_frame_size {
            return Err(protocol::FramingError::Oversized {
                claimed: payload_len,
                max: max_frame_size,
            });
        }
        Ok(framed)
    }

    /// Sends a message to all connected clients.
    /// Broken connections are tracked and cleaned up.
    fn send_to_all_clients(&mut self, msg: ServerMessage) {
        let serialized = match Self::frame_server_message(&msg) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(err = %err, "failed to serialize message for clients");
                return;
            }
        };

        let mut broken_clients: Vec<u64> = Vec::new();
        for (&client_id, client) in &mut self.clients {
            if let Some(writer) = &client.writer {
                if writer.control.send(serialized.clone()).is_err() {
                    debug!(client_id, "client writer channel closed during broadcast");
                    broken_clients.push(client_id);
                }
            }
        }

        // Remove broken clients.
        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    /// Sends a client-local side effect to the foreground client only.
    fn send_to_foreground_client(&mut self, msg: ServerMessage) -> bool {
        let Some(client_id) = self.foreground_client_id else {
            return false;
        };
        self.send_to_client(client_id, msg)
    }

    /// Sends a message to a specific client. Returns false if the client
    /// was not found or the send failed (client removed).
    fn send_to_client(&mut self, client_id: u64, msg: ServerMessage) -> bool {
        let serialized = match Self::frame_server_message(&msg) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(client_id, err = %err, "failed to serialize message for client");
                return false;
            }
        };

        if let Some(client) = self.clients.get(&client_id) {
            if let Some(writer) = &client.writer {
                if writer.control.send(serialized).is_err() {
                    debug!(
                        client_id,
                        "client writer channel closed during targeted send"
                    );
                    self.remove_client_and_resize_if_needed(client_id);
                    return false;
                }
            }
            true
        } else {
            false
        }
    }

    fn shutdown_terminal_stream_clients(&mut self, terminal_id: &str, reason: String) {
        let client_ids = terminal_stream_client_ids(&self.clients, terminal_id);

        for client_id in client_ids {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(reason.clone()),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    fn send_terminal_stream_detach_shutdown(&mut self, client_id: u64) {
        if matches!(
            self.clients.get(&client_id).map(|client| &client.mode),
            Some(
                ClientConnectionMode::TerminalAttach { .. }
                    | ClientConnectionMode::TerminalObserve { .. }
            )
        ) {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some("detached".to_owned()),
                },
            );
        }
    }

    #[cfg(unix)]
    fn disconnect_all_clients_for_handoff(&mut self) {
        let client_ids = self.clients.keys().copied().collect::<Vec<_>>();
        for client_id in client_ids {
            self.send_client_graphics_cleanup(client_id);
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(
                        "live update in progress; reconnect after handoff completes".to_owned(),
                    ),
                },
            );
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.writer = None;
            }
            let _ = self.remove_client(client_id);
        }
        self.foreground_client_id = None;
        self.sync_foreground_client_state();
        self.resize_shared_runtime_to_effective_size();
    }

    fn attach_terminal_client(
        &mut self,
        client_id: u64,
        terminal_id: String,
        takeover: bool,
    ) -> bool {
        if !self.client_is_pending_terminal_mode(client_id) {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(
                        "terminal attach failed: connection is not pending terminal attach"
                            .to_owned(),
                    ),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }

        let Some(real_terminal_id) = self.terminal_id_by_string(&terminal_id) else {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(format!(
                        "terminal attach failed: terminal {terminal_id} not found"
                    )),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        };

        if let Some(existing_owner) = self.terminal_attach_owners.get(&terminal_id).copied() {
            if existing_owner != client_id && !takeover {
                self.send_to_client(
                    client_id,
                    ServerMessage::ServerShutdown {
                        reason: Some(format!(
                            "terminal attach failed: terminal {terminal_id} already has an attached client; retry with --takeover"
                        )),
                    },
                );
                self.remove_client_and_resize_if_needed(client_id);
                return false;
            }
            if existing_owner != client_id {
                self.send_to_client(
                    existing_owner,
                    ServerMessage::ServerShutdown {
                        reason: Some("terminal attach taken over".to_owned()),
                    },
                );
                self.remove_client_and_resize_if_needed(existing_owner);
            }
        }

        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let (cols, rows) = client.terminal_size;
        let cell_size = client.cell_size;
        client.mode = ClientConnectionMode::TerminalAttach {
            terminal_id: terminal_id.clone(),
        };
        client.pending_terminal_attach = false;
        client.render_state.reset_baseline();
        client.last_activity = stamp;
        let was_foreground = self.foreground_client_id == Some(client_id);
        if was_foreground {
            self.promote_latest_remaining_client();
        }

        info!(client_id, cols, rows, terminal_id = %terminal_id, "terminal attach client connected");
        self.terminal_attach_owners
            .insert(terminal_id.clone(), client_id);
        self.app
            .state
            .direct_attach_resize_locks
            .insert(real_terminal_id.clone());
        self.app
            .start_pending_agent_resume_for_terminal(&real_terminal_id, rows, cols, true);
        if let Some(runtime) = self.app.terminal_runtimes.get(&real_terminal_id) {
            runtime.resize(rows, cols, cell_size.width_px, cell_size.height_px);
        }
        true
    }

    fn client_is_pending_terminal_mode(&self, client_id: u64) -> bool {
        self.clients.get(&client_id).is_some_and(|client| {
            client.pending_terminal_attach && matches!(client.mode, ClientConnectionMode::App)
        })
    }

    /// Handles a server event. Returns true if the event requires a re-render.
    fn handle_client_input_events(
        &mut self,
        client_id: u64,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> bool {
        let source_was_foreground = self.foreground_client_id == Some(client_id);
        let source_is_full_app = self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_full_app_client);
        let host_surface_redraw = crate::raw_input::events_require_host_surface_redraw(
            &events,
            self.app.state.redraw_on_focus_gained,
        );
        if let Some(client) = self.clients.get_mut(&client_id) {
            if host_surface_redraw {
                client.request_full_redraw();
                client.defer_full_render();
            } else {
                // Ensure semantic clients receive one post-input frame even if the
                // semantic buffer compares equal. Terminal-ANSI clients must keep their
                // server-side blit baseline; resetting it here forces a full redraw on
                // every keypress and makes remote sessions feel extremely slow.
                client.request_semantic_redraw_after_input();
            }
        }
        if source_is_full_app {
            self.update_client_outer_focus_from_events(client_id, &events);
        }
        let events = events_for_app_routing(events, source_was_foreground, source_is_full_app);
        let interaction = events_include_interaction(&events);
        let foreground_changed = if interaction {
            self.promote_client_to_foreground(client_id)
        } else {
            false
        };
        if foreground_changed {
            self.resize_shared_runtime_to_effective_size_before_input();
        }
        let theme_changed = self.update_client_host_theme_from_events(client_id, &events);
        self.app
            .route_client_events(events, self.foreground_client_id == Some(client_id));
        if self.app.take_config_reloaded_from_disk() {
            self.reload_server_config(false);
        } else {
            self.sync_foreground_client_state();
        }

        if self.app.state.detach_requested {
            self.app.state.detach_requested = false;
            info!(client_id, "client detach requested via keybind");

            self.send_client_graphics_cleanup(client_id);
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some("detached".to_owned()),
                },
            );

            if let Some(client) = self.clients.get_mut(&client_id) {
                client.writer = None;
            }

            false
        } else {
            foreground_changed || theme_changed || interaction
        }
    }

    fn handle_server_event(&mut self, ev: ServerEvent) -> bool {
        if self.handoff_in_progress && Self::ignore_client_event_during_handoff(&ev) {
            return false;
        }

        match ev {
            ServerEvent::ClientConnected {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                keybindings,
                writer,
                render_encoding,
                direct_attach_requested,
            } => {
                if self.handoff_in_progress {
                    if let Ok(message) =
                        Self::frame_server_message(&ServerMessage::ServerShutdown {
                            reason: Some(
                                "live update in progress; reconnect after handoff completes"
                                    .to_owned(),
                            ),
                        })
                    {
                        let _ = writer.control.send(message);
                    }
                    return false;
                }
                let first_app_client = !direct_attach_requested && self.app_client_count() == 0;
                info!(
                    client_id,
                    cols,
                    rows,
                    cell_width_px,
                    cell_height_px,
                    ?render_encoding,
                    "client connected"
                );
                let last_activity = self.allocate_activity_stamp();
                self.clients.insert(
                    client_id,
                    ClientConnection::new_with_mode(
                        ClientConnectionMode::App,
                        keybindings,
                        (cols, rows),
                        crate::kitty_graphics::HostCellSize {
                            width_px: cell_width_px,
                            height_px: cell_height_px,
                        },
                        crate::terminal_theme::TerminalTheme::default(),
                        None,
                        last_activity,
                        render_encoding,
                        direct_attach_requested,
                        Some(writer),
                    ),
                );
                if !direct_attach_requested {
                    self.foreground_client_id = Some(client_id);
                }
                if first_app_client {
                    self.app.mark_git_status_refresh_due(Instant::now());
                }
                self.sync_foreground_client_state();
                self.resize_shared_runtime_to_effective_size();
                self.nudge_handoff_panes_on_first_client_attach();
                true
            }
            ServerEvent::ClientAttachTerminal {
                client_id,
                terminal_id,
                takeover,
            } => self.attach_terminal_client(client_id, terminal_id, takeover),
            ServerEvent::ClientObserveTerminal { client_id, target } => {
                self.observe_terminal_client(client_id, target)
            }
            ServerEvent::ClientControlTerminal {
                client_id,
                target,
                takeover,
            } => self.control_terminal_client(client_id, target, takeover),
            ServerEvent::ClientAttachScroll {
                client_id,
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            } => self.handle_terminal_attach_scroll(
                client_id, source, direction, lines, column, row, modifiers,
            ),
            ServerEvent::ClientInput { client_id, data } => {
                if self.handoff_in_progress {
                    debug!(
                        client_id,
                        len = data.len(),
                        "ignored client input during handoff"
                    );
                    return false;
                }
                debug!(client_id, len = data.len(), "client input received");
                if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalAttach { terminal_id },
                    ..
                }) = self.clients.get(&client_id)
                {
                    if let Some(runtime) = self.runtime_for_terminal_id_string(terminal_id) {
                        if let Err(err) = apply_terminal_attach_input(runtime, data) {
                            warn!(client_id, terminal_id = %terminal_id, err = %err);
                        }
                    }
                    return true;
                }
                if matches!(
                    self.clients.get(&client_id).map(|client| &client.mode),
                    Some(ClientConnectionMode::TerminalObserve { .. })
                ) {
                    return false;
                }
                let events = if let Some(client) = self.clients.get_mut(&client_id) {
                    let mut events = client.raw_input.push(&data);
                    // The thin client only forwards a bare ESC after its local input timeout.
                    if data.as_slice() == b"\x1b" {
                        events.extend(client.raw_input.flush_timeout());
                    }
                    events
                } else {
                    Vec::new()
                };
                self.handle_client_input_events(client_id, events)
            }
            ServerEvent::ClientInputEvents { client_id, events } => {
                if self.handoff_in_progress {
                    debug!(
                        client_id,
                        len = events.len(),
                        "ignored client input events during handoff"
                    );
                    return false;
                }
                debug!(
                    client_id,
                    len = events.len(),
                    "client input events received"
                );
                if matches!(
                    self.clients.get(&client_id).map(|client| &client.mode),
                    Some(ClientConnectionMode::TerminalObserve { .. })
                ) {
                    return false;
                }
                let events = events
                    .iter()
                    .map(crate::protocol::ClientInputEvent::to_raw_input_event)
                    .collect();
                self.handle_client_input_events(client_id, events)
            }
            ServerEvent::ClientClipboardImage {
                client_id,
                extension,
                data,
            } => {
                debug!(
                    client_id,
                    len = data.len(),
                    extension = %extension,
                    "client clipboard image received"
                );
                if matches!(
                    self.clients.get(&client_id).map(|client| &client.mode),
                    Some(ClientConnectionMode::TerminalObserve { .. })
                ) {
                    return false;
                }
                match self.write_client_clipboard_image(client_id, &extension, &data) {
                    Ok(path) => self.paste_client_clipboard_image_path(client_id, path),
                    Err(err) => {
                        warn!(client_id, err = %err, "failed to stage client clipboard image");
                        true
                    }
                }
            }
            ServerEvent::ClientResize {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            } => {
                info!(
                    client_id,
                    cols, rows, cell_width_px, cell_height_px, "client resize"
                );
                let direct_terminal_id = if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalAttach { terminal_id },
                    terminal_size,
                    cell_size,
                    render_state,
                    ..
                }) = self.clients.get_mut(&client_id)
                {
                    *terminal_size = (cols, rows);
                    *cell_size = crate::kitty_graphics::HostCellSize {
                        width_px: cell_width_px,
                        height_px: cell_height_px,
                    };
                    render_state.reset_baseline();
                    Some(terminal_id.clone())
                } else {
                    None
                };
                if let Some(terminal_id) = direct_terminal_id {
                    if let Some(runtime) = self.runtime_for_terminal_id_string(&terminal_id) {
                        runtime.resize(rows, cols, cell_width_px, cell_height_px);
                    }
                    return true;
                }
                if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalObserve { .. },
                    terminal_size,
                    cell_size,
                    render_state,
                    ..
                }) = self.clients.get_mut(&client_id)
                {
                    *terminal_size = (cols, rows);
                    *cell_size = crate::kitty_graphics::HostCellSize {
                        width_px: cell_width_px,
                        height_px: cell_height_px,
                    };
                    render_state.reset_baseline();
                    return true;
                }
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.terminal_size = (cols, rows);
                    client.cell_size = crate::kitty_graphics::HostCellSize {
                        width_px: cell_width_px,
                        height_px: cell_height_px,
                    };
                }
                self.promote_client_to_foreground(client_id);
                self.resize_shared_runtime_to_effective_size();
                true
            }
            ServerEvent::ClientDetach { client_id } => {
                info!(client_id, "client detached");
                self.send_terminal_stream_detach_shutdown(client_id);
                self.remove_client_and_resize_if_needed(client_id);
                true
            }
            ServerEvent::ClientDisconnected { client_id } => {
                info!(client_id, "client disconnected");
                self.remove_client_and_resize_if_needed(client_id);
                true
            }
            ServerEvent::ClientWriterDrained { client_id } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                client.take_deferred_render() != DeferredRender::None
            }
            ServerEvent::QuitSignal => {
                // The quit check at the top of the loop handles this.
                // No render needed — the next iteration will initiate shutdown.
                false
            }
        }
    }

    fn handle_server_event_with_render_impact(&mut self, ev: ServerEvent) -> RenderImpact {
        let deferred_render = match &ev {
            ServerEvent::ClientWriterDrained { client_id } => self
                .clients
                .get(client_id)
                .map_or(DeferredRender::None, ClientConnection::deferred_render),
            _ => DeferredRender::None,
        };
        if !self.handle_server_event(ev) {
            return RenderImpact::None;
        }
        match deferred_render {
            DeferredRender::Graphics => RenderImpact::Graphics,
            DeferredRender::None | DeferredRender::Full => RenderImpact::Full,
        }
    }

    fn ignore_client_event_during_handoff(ev: &ServerEvent) -> bool {
        !matches!(
            ev,
            ServerEvent::ClientConnected { .. }
                | ServerEvent::ClientDisconnected { .. }
                | ServerEvent::ClientWriterDrained { .. }
                | ServerEvent::QuitSignal
        )
    }

    /// Drains API requests with shutdown awareness.
    ///
    /// During shutdown, remaining requests get a `server_unavailable` error.
    fn drain_api_requests_with_shutdown_check(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.app.api_rx.try_recv() {
            changed |= self.handle_api_request_with_shutdown_check(msg);
        }
        changed
    }

    fn drain_api_requests_with_render_impact(&mut self) -> RenderImpact {
        let mut impact = RenderImpact::None;
        while let Ok(msg) = self.app.api_rx.try_recv() {
            impact.merge(self.handle_api_request_with_render_impact(msg));
        }
        impact
    }

    /// Handles a single API request with shutdown awareness.
    ///
    /// Also forwards any toast/sound notifications that result from the API
    /// request to connected clients. API methods like `pane.report_agent`
    /// trigger internal events that may set toast state or would normally
    /// play sounds — in headless mode we forward these to clients instead.
    fn handle_api_request_with_shutdown_check(&mut self, msg: api::ApiRequestMessage) -> bool {
        self.handle_api_request_with_shutdown_check_inner(msg, false)
    }

    fn handle_api_request_with_render_impact(
        &mut self,
        msg: api::ApiRequestMessage,
    ) -> RenderImpact {
        if matches!(
            &msg.request.method,
            api::schema::Method::PaneGraphicsStreamSet(_)
        ) {
            return self.handle_pane_graphics_stream_frame(msg);
        }
        if self.handle_api_request_with_shutdown_check_inner(msg, false) {
            RenderImpact::Full
        } else {
            RenderImpact::None
        }
    }

    fn handle_api_request_with_shutdown_check_inner(
        &mut self,
        msg: api::ApiRequestMessage,
        skip_default_workspace_for_request: bool,
    ) -> bool {
        if self.shutting_down {
            // During shutdown, respond with server_unavailable.
            let response = serde_json::to_string(&api::schema::ErrorResponse {
                id: msg.request.id,
                error: api::schema::ErrorBody {
                    code: "server_unavailable".into(),
                    message: "server is shutting down".into(),
                },
            })
            .unwrap_or_else(|_| {
                r#"{"id":"","error":{"code":"server_unavailable","message":"server is shutting down"}}"#
                    .to_string()
            });
            let _ = msg.respond_to.send(response);
            return false;
        }

        let metadata_expired = self.app.expire_due_metadata(Instant::now());

        if let api::schema::Method::ServerLiveHandoff(params) = &msg.request.method {
            let handoff_result = self.perform_live_handoff(params.clone());
            let handoff_succeeded = handoff_result.is_ok();
            let response = match handoff_result {
                Ok(()) => serde_json::to_string(&api::schema::SuccessResponse {
                    id: msg.request.id,
                    result: api::schema::ResponseResult::Ok {},
                }),
                Err(err) => serde_json::to_string(&api::schema::ErrorResponse {
                    id: msg.request.id,
                    error: api::schema::ErrorBody {
                        code: "handoff_failed".into(),
                        message: err.to_string(),
                    },
                }),
            }
            .unwrap_or_else(|_| "{}".to_string());
            let _ = msg.respond_to.send(response);
            if handoff_succeeded {
                wait_for_live_handoff_response_write(msg.response_write_complete);
                self.finish_live_handoff_shutdown();
            }
            return true;
        }

        if let api::schema::Method::NotificationShow(params) = &msg.request.method {
            let response =
                self.handle_notification_show_api(msg.request.id.clone(), params.clone());
            let _ = msg.respond_to.send(response);
            return true;
        }

        if let api::schema::Method::MissionStart(params) = &msg.request.method {
            let response = self.handle_mission_start_api(&msg.request.id, params);
            let _ = msg.respond_to.send(response);
            return self.sync_mission_projection();
        }

        if let api::schema::Method::MissionHandoffStart(params) = &msg.request.method {
            let response = self.handle_mission_handoff_start_api(&msg.request.id, params);
            let _ = msg.respond_to.send(response);
            return self.sync_mission_projection();
        }

        if let api::schema::Method::MissionRespond(params) = &msg.request.method {
            let response = self.handle_mission_respond_api(&msg.request.id, params);
            let _ = msg.respond_to.send(response);
            return self.sync_mission_projection();
        }

        if let api::schema::Method::MissionClose(target) = &msg.request.method {
            let response = self.handle_mission_close_api(&msg.request.id, target);
            let _ = msg.respond_to.send(response);
            return self.sync_mission_projection();
        }

        if let api::schema::Method::AttentionList(params) = &msg.request.method {
            let changed = self.sync_mission_projection();
            let response = self.handle_attention_list_api(&msg.request.id, params);
            let _ = msg.respond_to.send(response);
            return changed;
        }

        if let api::schema::Method::AttentionGet(target) = &msg.request.method {
            let changed = self.sync_mission_projection();
            let response = self.handle_attention_get_api(&msg.request.id, target);
            let _ = msg.respond_to.send(response);
            return changed;
        }

        if let Some(outcome) = crate::server::mission_bridge::handle(
            &mut self.mission_runtime,
            &msg.request.id,
            &msg.request.method,
        ) {
            let _ = msg.respond_to.send(outcome.response);
            return outcome.changed | self.sync_mission_projection();
        }

        match &msg.request.method {
            api::schema::Method::ClientWindowTitleSet(params) => {
                let response = self.handle_client_window_title_api(
                    msg.request.id.clone(),
                    Some(params.title.clone()),
                );
                let _ = msg.respond_to.send(response);
                return true;
            }
            api::schema::Method::ClientWindowTitleClear(_) => {
                let response = self.handle_client_window_title_api(msg.request.id.clone(), None);
                let _ = msg.respond_to.send(response);
                return true;
            }
            _ => {}
        }

        let pane_graphics_revision_before = matches!(
            &msg.request.method,
            api::schema::Method::PaneGraphicsSet(_)
                | api::schema::Method::PaneGraphicsClear(_)
                | api::schema::Method::PaneGraphicsStreamOpen(_)
                | api::schema::Method::PaneGraphicsStreamClose(_)
        )
        .then_some(self.app.state.pane_graphics_revision);
        let mut changed = metadata_expired
            | (pane_graphics_revision_before.is_none() && api::request_changes_ui(&msg.request));
        let skip_default_workspace = skip_default_workspace_for_request
            || matches!(
                &msg.request.method,
                api::schema::Method::ServerStop(_) | api::schema::Method::ServerLiveHandoff(_)
            );
        changed |= self.drain_all_internal_events_with_forwarding();

        // Capture toast and effective pane states before the API call so we can
        // forward resulting client-local notifications. API requests like
        // pane.report_agent trigger handle_internal_event internally, which
        // bypasses drain_internal_events_with_forwarding. Headless mode disables
        // local sound playback, so sound notifications need to be forwarded here.
        let toast_before = self.app.state.toast.clone();
        let pane_states_before: Vec<(
            usize,
            crate::layout::PaneId,
            crate::detect::AgentState,
            Option<String>,
        )> = {
            let terminals = &self.app.state.terminals;
            self.app
                .state
                .workspaces
                .iter()
                .enumerate()
                .flat_map(|(ws_idx, ws)| {
                    ws.tabs.iter().flat_map(move |tab| {
                        tab.panes.iter().filter_map(move |(&pane_id, pane)| {
                            terminals.get(&pane.attached_terminal_id).map(|terminal| {
                                (
                                    ws_idx,
                                    pane_id,
                                    terminal.state,
                                    terminal.effective_agent_label().map(str::to_string),
                                )
                            })
                        })
                    })
                })
                .collect()
        };

        self.sync_foreground_client_state();
        if matches!(
            &msg.request.method,
            api::schema::Method::WorktreeCreate(_) | api::schema::Method::WorktreeRemove(_)
        ) {
            let deferred_changed = self
                .app
                .handle_deferred_worktree_api_request(msg.request, msg.respond_to);
            return changed | deferred_changed;
        }
        let response = if matches!(
            &msg.request.method,
            api::schema::Method::ServerReloadConfig(_)
        ) {
            let report = self.reload_server_config(true);
            serde_json::to_string(&api::schema::SuccessResponse {
                id: msg.request.id.clone(),
                result: api::schema::ResponseResult::ConfigReload {
                    status: report.status,
                    diagnostics: report.diagnostics,
                },
            })
            .unwrap_or_else(|err| {
                serde_json::to_string(&api::schema::ErrorResponse {
                    id: String::new(),
                    error: api::schema::ErrorBody {
                        code: "serialization_error".into(),
                        message: err.to_string(),
                    },
                })
                .unwrap_or_else(|_| "{}".to_string())
            })
        } else {
            self.app
                .handle_api_request_after_internal_events_drained(msg.request)
        };
        let _ = msg.respond_to.send(response);

        if let Some(revision_before) = pane_graphics_revision_before {
            changed |= revision_before != self.app.state.pane_graphics_revision;
        }

        // Forward new toast state only when a client-local delivery mode is selected.
        // Nagi delivery renders the toast in-frame and must not ask clients to
        // show a terminal or system notification.
        let toast_after = self.app.state.toast.clone();
        let forwarded_toast_from_state = if should_forward_toast_to_clients(
            self.app.state.toast_config.delivery,
        ) && toast_after.is_some()
            && toast_after != toast_before
        {
            if let Some(toast) = &toast_after {
                debug!(title = %toast.title, body = %toast.context, "forwarding toast notification from API request");
                self.send_notify_to_foreground_client(
                    toast_notify_kind(self.app.state.toast_config.delivery)
                        .expect("toast forwarding requires a client notification kind"),
                    &toast.title,
                    non_empty_body(&toast.context),
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        // Forward notifications for effective pane state changes that occurred
        // during the API request. Hook authority is already folded into
        // pane.state, so raw hook transitions must not produce separate sounds.
        for (ws_idx, pane_id, prev_state, prev_agent_label) in &pane_states_before {
            let pane_after = self
                .app
                .state
                .workspaces
                .get(*ws_idx)
                .and_then(|ws| ws.tabs.iter().find_map(|tab| tab.panes.get(pane_id)));

            let Some(pane_after) = pane_after else {
                continue;
            };

            let Some(terminal_after) = self
                .app
                .state
                .terminals
                .get(&pane_after.attached_terminal_id)
            else {
                continue;
            };

            let new_state = terminal_after.state;
            if new_state == *prev_state {
                continue;
            }

            let is_active_tab = self.app.state.pane_is_in_active_tab(*ws_idx, *pane_id);
            let suppress_active_tab_notifications =
                self.active_tab_suppresses_notifications(is_active_tab);

            let agent = terminal_after.effective_known_agent();
            let agent_label = terminal_after.effective_agent_label().map(str::to_string);

            debug!(
                ws_idx,
                pane_id = pane_id.raw(),
                prev_state = ?prev_state,
                new_state = ?new_state,
                agent = ?agent,
                "pane effective state changed during API request, checking notification"
            );

            if !forwarded_toast_from_state
                && self.app.state.toast_config.delay_seconds == 0
                && should_forward_toast_to_clients(self.app.state.toast_config.delivery)
            {
                if let Some(kind) =
                    crate::app::actions::notification_toast_for_state_change_with_agent_labels(
                        suppress_active_tab_notifications,
                        *prev_state,
                        new_state,
                        prev_agent_label.as_deref(),
                        agent_label.as_deref(),
                    )
                {
                    if let Some(agent_label) = self
                        .app
                        .state
                        .terminals
                        .get(&pane_after.attached_terminal_id)
                        .and_then(|terminal| terminal.effective_agent_label())
                    {
                        let event_text = match kind {
                            crate::app::state::ToastKind::NeedsAttention => "needs attention",
                            crate::app::state::ToastKind::Finished => "finished",
                            crate::app::state::ToastKind::UpdateInstalled => "updated",
                        };
                        let workspace_label = self.app.state.workspaces[*ws_idx].display_name_from(
                            &self.app.state.terminals,
                            &self.app.terminal_runtimes,
                        );
                        let context = crate::app::actions::notification_context(
                            &self.app.state.workspaces[*ws_idx],
                            &workspace_label,
                            *ws_idx,
                            *pane_id,
                        );
                        self.send_notify_to_foreground_client(
                            toast_notify_kind(self.app.state.toast_config.delivery)
                                .expect("toast forwarding requires a client notification kind"),
                            format!("{agent_label} {event_text}"),
                            non_empty_body(&context),
                        );
                    }
                }
            }

            // Forward sound notification when server-side sound policy allows it.
            // Clients still decide locally whether they can execute the side effect.
            if self.app.state.toast_config.delay_seconds == 0 && self.app.state.sound.allows(agent)
            {
                if let Some(sound) =
                    crate::app::actions::notification_sound_for_state_change_with_agent_labels(
                        suppress_active_tab_notifications,
                        *prev_state,
                        new_state,
                        prev_agent_label.as_deref(),
                        agent_label.as_deref(),
                    )
                {
                    debug!(sound = ?sound, "forwarding sound notification from API request");
                    self.send_notify_to_foreground_client(
                        protocol::NotifyKind::Sound,
                        sound_notify_message(sound),
                        None,
                    );
                }
            }
        }

        if !skip_default_workspace && latest_app_client(&self.clients).is_some() {
            changed |= self.app.ensure_default_workspace();
        }

        changed
    }

    fn stream_host_mouse_capture_mode(&mut self) {
        let enabled = self
            .app
            .state
            .should_capture_host_mouse_from(&self.app.terminal_runtimes);
        let serialized = match Self::frame_server_message(&ServerMessage::MouseCapture { enabled })
        {
            Ok(framed) => framed,
            Err(err) => {
                warn!(err = %err, "failed to serialize mouse capture mode for clients");
                return;
            }
        };

        let mut broken_clients: Vec<u64> = Vec::new();
        for (&client_id, client) in &mut self.clients {
            if !client.is_full_app_client() {
                continue;
            }
            if client.host_mouse_capture_active == Some(enabled) {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            if writer.control.send(serialized.clone()).is_err() {
                debug!(
                    client_id,
                    "client writer channel closed during mouse capture update"
                );
                broken_clients.push(client_id);
                continue;
            }
            client.host_mouse_capture_active = Some(enabled);
        }

        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    fn render_retained_pty_update_and_stream(&mut self) -> bool {
        crate::render_prof::event("retained.attempt");
        let retained_started = crate::render_prof::timer();
        macro_rules! retained_fallback {
            ($reason:literal) => {{
                crate::render_prof::event(concat!("retained_fallback.", $reason));
                crate::render_prof::duration_since("retained.total", retained_started);
                return false;
            }};
        }
        macro_rules! retained_success {
            ($reason:literal) => {{
                crate::render_prof::event("retained.success");
                crate::render_prof::event(concat!("retained_success.", $reason));
                crate::render_prof::duration_since("retained.total", retained_started);
                return true;
            }};
        }

        if !self.retained_pty_update_allowed_by_app_state() {
            retained_fallback!("unsafe_app_state");
        }

        let render_targets = render_targets(&self.clients, self.foreground_client_id);
        let [(client_id, (cols, rows), cell_size, _is_foreground, mode)] =
            render_targets.as_slice()
        else {
            retained_fallback!("multiple_or_no_target");
        };
        if !matches!(mode, ClientConnectionMode::App) {
            retained_fallback!("not_app_client");
        }
        let Some(client) = self.clients.get(client_id) else {
            retained_fallback!("client_missing");
        };
        if client.deferred_render() != DeferredRender::None {
            retained_fallback!("render_pending");
        }
        if self.app.state.kitty_graphics_enabled && !client.graphics_cache.is_empty() {
            retained_fallback!("graphics_cache_active");
        }
        if client.graphics_surface_reset_pending {
            retained_fallback!("graphics_surface_reset");
        }
        if self.app.state.kitty_graphics_enabled
            && cell_size.is_known()
            && crate::kitty_graphics::has_visible_pane_graphics(
                &self.app.state,
                &self.app.terminal_runtimes,
                *cell_size,
            )
        {
            retained_fallback!("visible_kitty_graphics");
        }
        let Some(mut frame) = client.render_state.last_frame().cloned() else {
            retained_fallback!("no_last_frame");
        };
        if frame.width != *cols || frame.height != *rows {
            retained_fallback!("frame_size_mismatch");
        }
        frame.graphics.clear();

        let Some(ws_idx) = self.app.state.active else {
            retained_fallback!("no_active_workspace");
        };
        let pane_infos = self.app.state.view.pane_infos.clone();
        if pane_infos.is_empty() {
            retained_fallback!("no_pane_info");
        }

        let mut touched = false;
        for info in pane_infos {
            if !rect_fits_frame(info.inner_rect, &frame) {
                retained_fallback!("pane_rect_outside_frame");
            }
            let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                ws_idx,
                info.id,
            ) else {
                retained_fallback!("missing_runtime");
            };
            match runtime.collect_dirty_patch(info.inner_rect.width, info.inner_rect.height) {
                crate::pane::TerminalDirtyPatchOutcome::Clean => {
                    crate::render_prof::event("retained.pane_clean");
                }
                crate::pane::TerminalDirtyPatchOutcome::Fallback => {
                    retained_fallback!("dirty_patch_fallback");
                }
                crate::pane::TerminalDirtyPatchOutcome::Patch(patch) => {
                    crate::render_prof::event("retained.pane_patch");
                    crate::render_prof::counter("retained.patch_rows", patch.rows.len() as u64);
                    if dirty_patch_intersects_hyperlinks(&frame, info.inner_rect, &patch) {
                        retained_fallback!("hyperlink_intersection");
                    }
                    if !apply_terminal_dirty_patch(&mut frame, info.inner_rect, patch) {
                        retained_fallback!("patch_apply_failed");
                    }
                    touched = true;
                }
            }
        }

        let previous_cursor = frame.cursor.clone();
        frame.cursor = crate::server::render_stream::focused_terminal_cursor(
            &self.app.state,
            &self.app.terminal_runtimes,
        );
        let cursor_changed = frame.cursor != previous_cursor;

        if !touched && !cursor_changed {
            retained_success!("clean_no_cursor_change");
        }

        let mut broken_clients = Vec::new();
        let sent = self.send_retained_frame_to_client(*client_id, frame, &mut broken_clients);
        for broken_client in broken_clients {
            self.remove_client_and_resize_if_needed(broken_client);
        }
        if sent {
            retained_success!("sent");
        }
        retained_fallback!("send_failed");
    }

    fn retained_pty_update_allowed_by_app_state(&self) -> bool {
        self.app.state.mode == app::Mode::Terminal
            && self.app.state.popup_pane.is_none()
            && self.app.state.selection.is_none()
            && self.app.state.copy_mode.is_none()
            && self.app.state.context_menu.is_none()
            && self.app.state.toast.is_none()
            && self.app.state.copy_feedback.is_none()
            && !self.app.full_redraw_pending
    }

    fn send_retained_frame_to_client(
        &mut self,
        client_id: u64,
        frame: FrameData,
        broken_clients: &mut Vec<u64>,
    ) -> bool {
        let Some(client) = self.clients.get_mut(&client_id) else {
            crate::render_prof::event("retained_send_fallback.client_missing");
            return false;
        };
        let Some(writer) = client.writer.as_ref().cloned() else {
            crate::render_prof::event("retained_send_fallback.writer_missing");
            return false;
        };
        let prepare_started = crate::render_prof::timer();
        let Some(prepared) = client.render_state.prepare_frame(frame) else {
            client.clear_deferred_render();
            crate::render_prof::event("retained_send.skip_identical");
            crate::render_prof::duration_since("retained_send.prepare_frame", prepare_started);
            return true;
        };
        crate::render_prof::duration_since("retained_send.prepare_frame", prepare_started);
        let serialize_started = crate::render_prof::timer();
        let serialized = match Self::frame_server_message(prepared.message()) {
            Ok(framed) => {
                crate::render_prof::duration_since("retained_send.serialize", serialize_started);
                framed
            }
            Err(protocol::FramingError::Oversized { claimed, max }) => {
                warn!(
                    client_id,
                    claimed, max, "skipping oversized retained frame for client"
                );
                crate::render_prof::event("retained_send_fallback.serialize_oversized");
                crate::render_prof::duration_since("retained_send.serialize", serialize_started);
                return false;
            }
            Err(err) => {
                warn!(client_id, err = %err, "failed to serialize retained frame for client");
                broken_clients.push(client_id);
                crate::render_prof::event("retained_send_fallback.serialize_error");
                crate::render_prof::duration_since("retained_send.serialize", serialize_started);
                return false;
            }
        };
        crate::render_prof::counter("retained_send.bytes", serialized.len() as u64);

        let send_started = crate::render_prof::timer();
        match writer.render.try_send(serialized) {
            Ok(()) => {
                client.clear_deferred_render();
                client.render_state.commit_sent_frame(prepared);
                crate::render_prof::event("retained_send.sent");
                crate::render_prof::duration_since("retained_send.try_send", send_started);
                true
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                client.defer_full_render();
                crate::render_prof::event("retained_send_fallback.queue_full");
                crate::render_prof::duration_since("retained_send.try_send", send_started);
                debug!(
                    client_id,
                    "render queue full, deferring latest retained frame"
                );
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                debug!(client_id, "client writer channel closed, marking as broken");
                broken_clients.push(client_id);
                crate::render_prof::event("retained_send_fallback.writer_disconnected");
                crate::render_prof::duration_since("retained_send.try_send", send_started);
                false
            }
        }
    }

    fn sync_mission_projection(&mut self) -> bool {
        let durable_missions = self.mission_runtime.missions();
        let missions = durable_missions
            .iter()
            .cloned()
            .map(crate::server::mission_bridge::mission_view)
            .collect::<Vec<_>>();
        let attention_items = self
            .mission_runtime
            .attention_items()
            .into_iter()
            .filter_map(|attention| self.attention_item_projection(&attention, &durable_missions))
            .collect::<Vec<_>>();
        let changed = self.app.state.set_mission_views(missions)
            | self.app.state.set_attention_items(attention_items);
        if changed && self.app.state.mode == app::Mode::Navigator {
            self.app
                .state
                .reconcile_navigator_selection_from(&self.app.terminal_runtimes);
        }
        changed
    }

    fn attention_item_projection(
        &self,
        attention: &crate::mission::store::DurableAttentionView,
        missions: &[crate::mission::store::MissionView],
    ) -> Option<crate::api::schema::AttentionItemV1> {
        use crate::{
            api::schema::{
                AttentionDeliveryStateV1, AttentionFailureCodeV1, AttentionItemV1, AttentionKindV1,
                AttentionPaneTargetV1, AttentionQuestionOptionV1, AttentionQuestionV1,
                AttentionResponseCapabilityV1, AttentionSourceV1, AttentionStateV1,
                ContractVersionV1,
            },
            mission::store::{PersistedAttentionState, PersistedResponseState},
        };

        let mission = missions
            .iter()
            .find(|mission| mission.mission_id == attention.mission_id)?;
        let run = mission.run.as_ref()?;
        let live = self.managed_runs.values().find_map(|managed| {
            (managed.mission_id == attention.mission_id)
                .then(|| managed.responses.get(&attention.attention_id))
                .flatten()
        });
        let requested_action = live
            .map(|response| response.requested_action.clone())
            .unwrap_or_else(|| "Review the provider request in its originating session".into());
        let created_at_millis = live
            .map(|response| response.created_at_millis)
            .unwrap_or(attention.updated_at_millis);
        let state = match attention.state {
            PersistedAttentionState::Open => AttentionStateV1::Open,
            PersistedAttentionState::PendingResponse => attention
                .response
                .as_ref()
                .map(|response| AttentionStateV1::PendingResponse {
                    decision: wire_attention_decision(response.decision),
                    actor: response.actor_id.clone(),
                    requested_at_millis: response.updated_at_millis,
                })
                .unwrap_or(AttentionStateV1::Open),
            PersistedAttentionState::ReconciliationRequired => attention
                .response
                .as_ref()
                .map(|response| AttentionStateV1::ReconciliationRequired {
                    decision: wire_attention_decision(response.decision),
                    actor: response.actor_id.clone(),
                    code: match response.state {
                        PersistedResponseState::ReconciliationRequired { code }
                        | PersistedResponseState::Failed { code } => wire_attention_failure(code),
                        PersistedResponseState::Requested
                        | PersistedResponseState::Acknowledged { .. } => {
                            AttentionFailureCodeV1::TransportClosed
                        }
                    },
                    at_millis: response.updated_at_millis,
                })
                .unwrap_or(AttentionStateV1::Open),
            PersistedAttentionState::Resolved => attention
                .response
                .as_ref()
                .map(|response| AttentionStateV1::Resolved {
                    decision: wire_attention_decision(response.decision),
                    actor: response.actor_id.clone(),
                    at_millis: response.updated_at_millis,
                })
                .unwrap_or_else(|| AttentionStateV1::Dismissed {
                    actor: "system".into(),
                    reason: "Provider request resolved".into(),
                    at_millis: attention.updated_at_millis,
                }),
            PersistedAttentionState::Dismissed => AttentionStateV1::Dismissed {
                actor: "system".into(),
                reason: "Request dismissed".into(),
                at_millis: attention.updated_at_millis,
            },
            PersistedAttentionState::Expired => AttentionStateV1::Expired {
                at_millis: attention.updated_at_millis,
            },
        };
        let delivery = attention
            .response
            .as_ref()
            .map(|response| match response.state {
                PersistedResponseState::Requested => AttentionDeliveryStateV1::Pending {
                    attempt: response.attempt,
                    requested_at_millis: response.updated_at_millis,
                },
                PersistedResponseState::Acknowledged { .. } => {
                    AttentionDeliveryStateV1::Acknowledged {
                        attempt: response.attempt,
                        at_millis: response.updated_at_millis,
                    }
                }
                PersistedResponseState::Failed { code } => {
                    AttentionDeliveryStateV1::DefinitelyNotApplied {
                        attempt: response.attempt,
                        code: wire_attention_failure(code),
                        at_millis: response.updated_at_millis,
                    }
                }
                PersistedResponseState::ReconciliationRequired { code } => {
                    AttentionDeliveryStateV1::DeliveryUnknown {
                        attempt: response.attempt,
                        code: wire_attention_failure(code),
                        at_millis: response.updated_at_millis,
                    }
                }
            })
            .unwrap_or_else(|| {
                if matches!(
                    attention.state,
                    PersistedAttentionState::Dismissed | PersistedAttentionState::Expired
                ) {
                    AttentionDeliveryStateV1::NotApplicable
                } else {
                    AttentionDeliveryStateV1::NotRequested
                }
            });
        Some(AttentionItemV1 {
            schema_version: ContractVersionV1,
            attention_id: attention.attention_id.clone(),
            mission_id: attention.mission_id.clone(),
            run_id: run.run_id.clone(),
            session_id: live
                .map(|response| response.session_id.clone())
                .or_else(|| run.provider_session_id.clone())
                .unwrap_or_else(|| run.run_id.clone()),
            pane: AttentionPaneTargetV1 {
                workspace_id: attention.mission_id.clone(),
                pane_id: run.run_id.clone(),
            },
            kind: live.map_or(
                AttentionKindV1::PermissionRequest,
                |response| match response.class {
                    crate::managed_provider::AttentionClass::UserInput => {
                        AttentionKindV1::ProviderQuestion
                    }
                    crate::managed_provider::AttentionClass::CommandApproval
                    | crate::managed_provider::AttentionClass::FileChangeApproval
                    | crate::managed_provider::AttentionClass::PermissionApproval => {
                        AttentionKindV1::PermissionRequest
                    }
                },
            ),
            requested_action: requested_action.clone(),
            scope: requested_action,
            risk: wire_attention_risk(attention.risk),
            provider: wire_mission_provider(run.provider),
            source: if live.is_some() {
                AttentionSourceV1::ProviderApi
            } else {
                AttentionSourceV1::Process
            },
            response_capability: if live.is_some() {
                AttentionResponseCapabilityV1::Reliable
            } else {
                AttentionResponseCapabilityV1::OpenPaneOnly
            },
            questions: live
                .map(|response| {
                    response
                        .questions
                        .iter()
                        .map(|question| AttentionQuestionV1 {
                            id: question.id.clone(),
                            header: question.header.clone(),
                            prompt: question.prompt.clone(),
                            options: question
                                .options
                                .iter()
                                .map(|option| AttentionQuestionOptionV1 {
                                    label: option.label.clone(),
                                    description: option.description.clone(),
                                })
                                .collect(),
                            multiple: question.multiple,
                            custom_allowed: question.custom_allowed,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            created_at_millis,
            expires_at_millis: None,
            occurrence_count: 1,
            unread: matches!(
                attention.state,
                PersistedAttentionState::Open
                    | PersistedAttentionState::PendingResponse
                    | PersistedAttentionState::ReconciliationRequired
            ),
            state,
            delivery,
        })
    }

    fn render_and_stream(&mut self) {
        self.sync_mission_projection();
        let full_started = crate::render_prof::timer();
        let render_targets = render_targets(&self.clients, self.foreground_client_id);

        if render_targets.is_empty() {
            let (cols, rows) = self.effective_size;
            let area = Rect::new(0, 0, cols, rows);
            let resize_panes = self.app.state.view.pane_infos.is_empty();
            let render_started = crate::render_prof::timer();
            let _ = crate::server::render_stream::render_virtual_with_runtime_registry(
                &mut self.app.state,
                &self.app.terminal_runtimes,
                area,
                resize_panes,
                crate::kitty_graphics::HostCellSize::default(),
            );
            crate::render_prof::duration_since("full_render.render_virtual", render_started);
            self.app.full_redraw_pending = false;
            crate::render_prof::duration_since("full_render.total", full_started);
            debug!(
                cols,
                rows, resize_panes, "rendered virtual frame with no attached clients"
            );
            return;
        }

        let mut broken_clients: Vec<u64> = Vec::new();
        let mut deferred_frame = false;
        for (client_id, (cols, rows), cell_size, is_foreground, mode) in render_targets {
            let area = Rect::new(0, 0, cols, rows);
            let is_app_client = matches!(mode, ClientConnectionMode::App);
            let mut frame = match mode {
                ClientConnectionMode::App => {
                    let render_started = crate::render_prof::timer();
                    let render_cell_size =
                        if self.app.state.kitty_graphics_enabled && cell_size.is_known() {
                            cell_size
                        } else {
                            crate::kitty_graphics::HostCellSize::default()
                        };
                    let (buffer, cursor) =
                        crate::server::render_stream::render_virtual_with_runtime_registry(
                            &mut self.app.state,
                            &self.app.terminal_runtimes,
                            area,
                            is_foreground,
                            render_cell_size,
                        );
                    crate::render_prof::duration_since(
                        "full_render.render_virtual",
                        render_started,
                    );
                    let hyperlinks_started = crate::render_prof::timer();
                    let hyperlinks = crate::server::render_stream::visible_hyperlinks(
                        &self.app.state,
                        &self.app.terminal_runtimes,
                    );
                    crate::render_prof::duration_since(
                        "full_render.visible_hyperlinks",
                        hyperlinks_started,
                    );
                    let frame_started = crate::render_prof::timer();
                    let frame = FrameData::from_ratatui_buffer_with_hyperlinks(
                        &buffer,
                        cursor,
                        &hyperlinks,
                    );
                    crate::render_prof::duration_since("full_render.frame_build", frame_started);
                    frame
                }
                ClientConnectionMode::TerminalAttach { terminal_id }
                | ClientConnectionMode::TerminalObserve { terminal_id } => {
                    let Some(runtime) = self.runtime_for_terminal_id_string(&terminal_id) else {
                        self.send_to_client(
                            client_id,
                            ServerMessage::ServerShutdown {
                                reason: Some(format!(
                                    "terminal attach ended: terminal {terminal_id} not found"
                                )),
                            },
                        );
                        broken_clients.push(client_id);
                        continue;
                    };
                    let render_started = crate::render_prof::timer();
                    let (buffer, cursor) =
                        crate::server::render_stream::render_terminal_virtual(runtime, area);
                    crate::render_prof::duration_since(
                        "full_render.render_terminal_virtual",
                        render_started,
                    );
                    let hyperlinks_started = crate::render_prof::timer();
                    let hyperlinks = runtime.visible_hyperlinks(area);
                    crate::render_prof::duration_since(
                        "full_render.visible_hyperlinks",
                        hyperlinks_started,
                    );
                    let frame_started = crate::render_prof::timer();
                    let frame = FrameData::from_ratatui_buffer_with_hyperlinks(
                        &buffer,
                        cursor,
                        &hyperlinks,
                    );
                    crate::render_prof::duration_since("full_render.frame_build", frame_started);
                    frame
                }
            };

            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            let mut next_graphics_cache = client.graphics_cache.clone();
            let graphics_surface_reset_pending = client.graphics_surface_reset_pending;
            if is_app_client && self.app.state.kitty_graphics_enabled && cell_size.is_known() {
                if graphics_surface_reset_pending {
                    frame.graphics = next_graphics_cache.clear_bytes();
                }
                let graphics_started = crate::render_prof::timer();
                frame
                    .graphics
                    .extend(crate::kitty_graphics::encode_local_pane_graphics(
                        &self.app.state,
                        &self.app.terminal_runtimes,
                        cell_size,
                        &mut next_graphics_cache,
                    ));
                crate::render_prof::duration_since("full_render.graphics_encode", graphics_started);
            } else {
                frame.graphics = next_graphics_cache.clear_bytes();
            }

            let Some(writer) = client.writer.as_ref().cloned() else {
                crate::render_prof::event("full_render.writer_missing");
                continue;
            };

            let mut commit_graphics_cache = true;
            if frame.graphics.len() > MAX_GRAPHICS_FRAME_SIZE {
                warn!(
                    client_id,
                    graphics_bytes = frame.graphics.len(),
                    max = MAX_GRAPHICS_FRAME_SIZE,
                    "dropping oversized graphics payload for client frame"
                );
                frame.graphics.clear();
                commit_graphics_cache = false;
            }

            let max_frame_size = if frame.graphics.is_empty() {
                MAX_FRAME_SIZE
            } else {
                MAX_GRAPHICS_FRAME_SIZE
            };
            let has_graphics = !frame.graphics.is_empty();
            let prepare_started = crate::render_prof::timer();
            let Some(mut prepared) = client.render_state.prepare_frame(frame) else {
                client.clear_deferred_render();
                crate::render_prof::event("full_render.skip_identical");
                crate::render_prof::duration_since("full_render.prepare_frame", prepare_started);
                continue;
            };
            crate::render_prof::duration_since("full_render.prepare_frame", prepare_started);

            let serialize_started = crate::render_prof::timer();
            let serialized = match Self::frame_server_message_with_max(
                prepared.message(),
                max_frame_size,
            ) {
                Ok(framed) => {
                    crate::render_prof::duration_since("full_render.serialize", serialize_started);
                    framed
                }
                Err(protocol::FramingError::Oversized { claimed, max }) if has_graphics => {
                    warn!(
                        client_id,
                        claimed, max, "dropping graphics from oversized frame for client"
                    );
                    let Some(mut text_only_frame) = prepared.into_frame() else {
                        crate::render_prof::event("full_render.serialize_error");
                        crate::render_prof::duration_since(
                            "full_render.serialize",
                            serialize_started,
                        );
                        continue;
                    };
                    text_only_frame.graphics.clear();
                    let Some(text_only_prepared) =
                        client.render_state.prepare_frame(text_only_frame)
                    else {
                        client.clear_deferred_render();
                        crate::render_prof::event("full_render.skip_identical_text_only");
                        crate::render_prof::duration_since(
                            "full_render.serialize",
                            serialize_started,
                        );
                        continue;
                    };
                    let framed = match Self::frame_server_message(text_only_prepared.message()) {
                        Ok(framed) => framed,
                        Err(err) => {
                            warn!(client_id, err = %err, "failed to serialize text-only frame for client");
                            broken_clients.push(client_id);
                            crate::render_prof::event("full_render.serialize_error");
                            crate::render_prof::duration_since(
                                "full_render.serialize",
                                serialize_started,
                            );
                            continue;
                        }
                    };
                    prepared = text_only_prepared;
                    commit_graphics_cache = false;
                    crate::render_prof::duration_since("full_render.serialize", serialize_started);
                    framed
                }
                Err(protocol::FramingError::Oversized { claimed, max }) => {
                    warn!(
                        client_id,
                        claimed, max, "skipping oversized frame for client"
                    );
                    crate::render_prof::event("full_render.serialize_oversized");
                    crate::render_prof::duration_since("full_render.serialize", serialize_started);
                    continue;
                }
                Err(err) => {
                    warn!(client_id, err = %err, "failed to serialize frame for client");
                    broken_clients.push(client_id);
                    crate::render_prof::event("full_render.serialize_error");
                    crate::render_prof::duration_since("full_render.serialize", serialize_started);
                    continue;
                }
            };
            crate::render_prof::counter("full_render.bytes", serialized.len() as u64);

            let send_started = crate::render_prof::timer();
            match writer.render.try_send(serialized) {
                Ok(()) => {
                    client.clear_deferred_render();
                    if commit_graphics_cache {
                        client.graphics_cache = next_graphics_cache;
                        client.graphics_surface_reset_pending = false;
                    }
                    client.render_state.commit_sent_frame(prepared);
                    crate::render_prof::event("full_render.sent");
                    crate::render_prof::duration_since("full_render.try_send", send_started);
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    client.defer_full_render();
                    deferred_frame = true;
                    crate::render_prof::event("full_render.queue_full");
                    crate::render_prof::duration_since("full_render.try_send", send_started);
                    debug!(client_id, "render queue full, deferring latest frame");
                    continue;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    debug!(client_id, "client writer channel closed, marking as broken");
                    broken_clients.push(client_id);
                    crate::render_prof::event("full_render.writer_disconnected");
                    crate::render_prof::duration_since("full_render.try_send", send_started);
                    continue;
                }
            }
        }

        if !broken_clients.is_empty() {
            for client_id in broken_clients {
                self.remove_client_and_resize_if_needed(client_id);
            }
        }

        let (cols, rows) = self.effective_size;
        if !deferred_frame {
            self.app.full_redraw_pending = false;
        }
        crate::render_prof::duration_since("full_render.total", full_started);
        debug!(cols, rows, foreground_client_id = ?self.foreground_client_id, "rendered virtual frame(s)");
    }

    /// Handle scheduled tasks for the headless server.
    ///
    /// Similar to `App::handle_scheduled_tasks` but without resize polling
    /// (the server doesn't have a terminal to resize).
    fn handle_scheduled_tasks_headless(&mut self, now: Instant, geometry_dirty: bool) -> bool {
        let mut changed = false;

        self.app.sync_headless_animation_timer(now);

        // No resize polling needed — server has no terminal.
        // Client resize messages drive size changes instead.

        if self
            .app
            .config_diagnostic_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.config_diagnostic_deadline = None;
            self.app.state.config_diagnostic = None;
            changed = true;
        }

        if self
            .app
            .toast_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.toast_deadline = None;
            self.app.state.toast = None;
            changed = true;
        }

        if self
            .app
            .state
            .next_pending_agent_notification_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            let previous_toast = self.app.state.toast.clone();
            let mut deliveries = self.app.state.drain_due_agent_notifications(now);
            if !deliveries.is_empty() {
                self.app
                    .refresh_agent_notification_delivery_contexts(&mut deliveries);
                self.app.sync_toast_deadline(previous_toast);
                for delivery in &deliveries {
                    self.forward_agent_notification_delivery(delivery);
                }
                changed = true;
            }
        }

        if self
            .app
            .copy_feedback_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.copy_feedback_deadline = None;
            self.app.state.copy_feedback = None;
            changed = true;
        }

        if self
            .app
            .next_animation_tick
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.state.spinner_tick = self
                .app
                .state
                .spinner_tick
                .wrapping_add(app::HEADLESS_ANIMATION_TICK_STEP);
            self.app.next_animation_tick = Some(now + app::HEADLESS_ANIMATION_INTERVAL);
            changed = true;
        }

        if self
            .app
            .selection_autoscroll_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.tick_selection_autoscroll(now);
            changed = true;
        }

        changed |= self.app.clear_due_selection_highlight(now);

        if self.has_app_client() {
            self.app.start_git_status_refresh_if_due(now);
        }

        if self
            .app
            .next_auto_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.run_auto_update_check();
        }

        if self
            .app
            .next_agent_manifest_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.run_agent_manifest_update_check();
        }

        if self
            .app
            .session_save_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.start_background_session_save();
        }

        if let Some(deadline) = self
            .app
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        {
            self.app.expire_metadata_at(deadline, now);
            changed = true;
        }

        if geometry_dirty || self.foreground_client_id.is_none() {
            self.app.pending_agent_resume_deadline = None;
        } else {
            self.app.sync_pending_agent_resume_deadline(now);
            changed |= self
                .app
                .start_pending_agent_resumes(self.app.pending_agent_resume_due(now));
        }
        self.app.sync_headless_animation_timer(now);
        changed
    }

    /// Initiates graceful shutdown.
    fn initiate_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        info!("server shutdown initiated");
        self.shutting_down = true;

        // Clear client-local host graphics, then send ServerShutdown to all connected clients.
        self.send_all_clients_graphics_cleanup();
        let shutdown_msg = ServerMessage::ServerShutdown {
            reason: Some("server is shutting down".to_owned()),
        };
        self.send_to_all_clients(shutdown_msg);

        // Give client writer threads a moment to flush the shutdown message.
        // A short sleep ensures the message is written to the socket before
        // we close the connections.
        std::thread::sleep(Duration::from_millis(50));

        // Signal the main loop to exit.
        self.should_quit.store(true, Ordering::Release);
        self.app.state.should_quit = true;
    }

    /// Completes the shutdown sequence: send ServerShutdown to clients,
    /// close client connections, remove socket files, and clean up.
    fn complete_shutdown(&mut self) -> io::Result<()> {
        info!("completing server shutdown");

        // Send ServerShutdown to all remaining clients.
        if !self.clients.is_empty() {
            self.send_all_clients_graphics_cleanup();
            let shutdown_msg = ServerMessage::ServerShutdown {
                reason: Some("server is shutting down".to_owned()),
            };
            self.send_to_all_clients(shutdown_msg);

            // Give writer threads a moment to flush before closing.
            std::thread::sleep(Duration::from_millis(50));
        }

        // Drain remaining API requests with server_unavailable.
        self.drain_api_requests_with_shutdown_check();

        // Close all client connections.
        let staged_files = self
            .clients
            .drain()
            .flat_map(|(_, client)| client.staged_clipboard_files)
            .collect::<Vec<_>>();
        crate::server::clipboard_image::remove_files(staged_files);

        // Remove socket files.
        self.cleanup_sockets()?;

        Ok(())
    }

    /// Removes socket files created by the server.
    fn cleanup_sockets(&self) -> io::Result<()> {
        if let Err(err) =
            remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity)
        {
            if err.kind() != io::ErrorKind::NotFound {
                warn!(
                    path = %self.client_socket_path.display(),
                    err = %err,
                    "failed to remove client socket on shutdown"
                );
            }
        }
        Ok(())
    }
}

fn events_for_app_routing(
    events: Vec<crate::raw_input::RawInputEvent>,
    mut source_is_foreground: bool,
    source_is_full_app: bool,
) -> Vec<crate::raw_input::RawInputEvent> {
    events
        .into_iter()
        .filter_map(|event| match event {
            crate::raw_input::RawInputEvent::OuterFocusGained
            | crate::raw_input::RawInputEvent::OuterFocusLost
                if !source_is_full_app =>
            {
                None
            }
            crate::raw_input::RawInputEvent::OuterFocusGained => {
                source_is_foreground = true;
                Some(event)
            }
            crate::raw_input::RawInputEvent::OuterFocusLost if !source_is_foreground => None,
            crate::raw_input::RawInputEvent::Key(_)
            | crate::raw_input::RawInputEvent::Mouse(_)
            | crate::raw_input::RawInputEvent::Paste(_) => {
                source_is_foreground = true;
                Some(event)
            }
            _ => Some(event),
        })
        .collect()
}

impl Drop for HeadlessServer {
    fn drop(&mut self) {
        let managed_run_ids = self.managed_runs.keys().cloned().collect::<Vec<_>>();
        let at_millis = crate::server::mission_bridge::now_millis();
        for run_id in managed_run_ids {
            self.mark_inflight_delivery_unknown(
                &run_id,
                crate::mission::store::ResponseFailureCode::TransportClosed,
                at_millis,
            );
            self.release_managed_run(&run_id);
        }
        for (_, pending) in self.pending_proofs.drain() {
            pending.cancelled.store(true, Ordering::Release);
            let _ = self.mission_runtime.release_worktree(&pending.lease);
        }
        let staged_files = self
            .clients
            .drain()
            .flat_map(|(_, client)| client.staged_clipboard_files)
            .collect::<Vec<_>>();
        crate::server::clipboard_image::remove_files(staged_files);
        let _ = self.cleanup_sockets();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Installs a Ctrl+C handler that sets the should_quit flag and wakes up
/// the event loop by sending a QuitSignal on the server event channel.
fn ctrlc_handler(should_quit: Arc<AtomicBool>, server_event_tx: mpsc::Sender<ServerEvent>) {
    let _ = ctrlc::set_handler(move || {
        should_quit.store(true, Ordering::Release);
        // Wake up the event loop so the quit flag is checked promptly.
        let _ = server_event_tx.try_send(ServerEvent::QuitSignal);
    });
}

/// Sleep until a deadline, or return pending if none.
async fn sleep_until_or_pending(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

fn sanitize_notification_text(value: &str, max_chars: usize) -> Option<String> {
    let mut sanitized = String::new();
    let mut previous_space = false;
    for ch in value.chars() {
        let replacement = if ch == '\n' || ch == '\r' || ch == '\t' {
            Some(' ')
        } else if ch.is_control() {
            None
        } else {
            Some(ch)
        };
        let Some(ch) = replacement else {
            continue;
        };
        if ch.is_whitespace() {
            if previous_space {
                continue;
            }
            previous_space = true;
            sanitized.push(' ');
        } else {
            previous_space = false;
            sanitized.push(ch);
        }
        if sanitized.chars().count() >= max_chars {
            break;
        }
    }
    let sanitized = sanitized.trim().to_string();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn sanitize_window_title_text(value: &str, max_chars: usize) -> Option<String> {
    let sanitized = value
        .chars()
        .filter(|ch| !matches!(*ch, '\u{1b}' | '\u{7}' | '\u{9c}') && !ch.is_control())
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn server_config_diagnostic_summaries(diagnostics: &[String]) -> (Option<String>, Option<String>) {
    let without_keybindings = diagnostics
        .iter()
        .filter(|diagnostic| !is_keybinding_config_diagnostic(diagnostic))
        .cloned()
        .collect::<Vec<_>>();
    (
        config::config_diagnostic_summary(diagnostics),
        config::config_diagnostic_summary(&without_keybindings),
    )
}

fn is_keybinding_config_diagnostic(diagnostic: &str) -> bool {
    diagnostic.contains("keybinding") || diagnostic.contains("keys.")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the headless server. This is the entry point called from main.rs.
pub fn run_server() -> io::Result<()> {
    init_logging();
    crate::platform::raise_server_nofile_limit();

    let args: Vec<String> = std::env::args().collect();
    if args.get(2).map(String::as_str) == Some("--handoff-import") {
        let socket_path = args
            .get(3)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff socket"))?;
        let token = args
            .get(4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff token"))?;
        return run_handoff_import_server(&socket_path, token);
    }

    let loaded_config = config::Config::load();
    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();

    // Start the JSON API socket server.
    let _api_server = match api::start_server(api_tx.clone(), event_hub.clone()) {
        Ok(server) => server,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            eprintln!("error: nagi server is already running");
            eprintln!("api socket: {}", api::socket_path().display());
            std::process::exit(1);
        }
        Err(err) => return Err(err),
    };

    let no_session = false; // Server always does session persistence.

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        // Create the App (with AppState, event channels, etc.).
        let mut app = app::App::new(
            &loaded_config.config,
            no_session,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub,
        );
        seed_startup_workspace_if_empty(&mut app);

        // The server runs headless — disable local notification side effects.
        // Sound and terminal notifications are forwarded to connected clients
        // as ServerMessage::Notify instead of emitted by the server process.
        // The prefix input-source switch is likewise forwarded to the foreground
        // client (ServerMessage::PrefixInputSource), never applied in-process.
        app.state.local_sound_playback = false;
        app.local_terminal_notifications = false;
        app.local_input_source_switch = false;

        // Create the headless server.
        let mut server = match HeadlessServer::new(
            app,
            &loaded_config.diagnostics,
            Some(api_tx.clone()),
            Some(_api_server),
        ) {
            Ok(server) => server,
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                eprintln!("error: nagi server is already running");
                eprintln!("client socket: {}", client_socket_path().display());
                std::process::exit(1);
            }
            Err(err) => return Err(err),
        };

        info!(
            api_socket = %api::socket_path().display(),
            client_socket = %client_socket_path().display(),
            "nagi server started"
        );
        print_ready_message(&api::socket_path(), &client_socket_path());

        server.run().await
    });

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

fn seed_startup_workspace_if_empty(app: &mut app::App) {
    let Some(cwd) = take_startup_cwd() else {
        return;
    };

    seed_startup_workspace(app, cwd);
}

fn seed_startup_workspace(app: &mut app::App, cwd: PathBuf) {
    if !app.state.workspaces.is_empty() {
        info!(
            cwd = %cwd.display(),
            "restored session already has workspaces; ignoring startup cwd"
        );
        return;
    }

    let previous_mode = app.state.mode;
    let preserve_mode = matches!(
        previous_mode,
        app::Mode::Onboarding
            | app::Mode::ReleaseNotes
            | app::Mode::ProductAnnouncement
            | app::Mode::Settings
    );

    match app.create_workspace_with_options(cwd.clone(), true) {
        Ok(_) => {
            if preserve_mode {
                app.state.mode = previous_mode;
            }
            info!(cwd = %cwd.display(), "created startup workspace");
        }
        Err(err) => {
            warn!(cwd = %cwd.display(), err = %err, "failed to create startup workspace");
            app.state.mode = app::Mode::Navigate;
        }
    }
}

fn take_startup_cwd() -> Option<PathBuf> {
    let cwd = std::env::var_os(crate::server::autodetect::STARTUP_CWD_ENV_VAR)?;
    std::env::remove_var(crate::server::autodetect::STARTUP_CWD_ENV_VAR);
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

#[cfg(unix)]
fn run_handoff_import_server(socket_path: &Path, token: &str) -> io::Result<()> {
    let loaded_config = config::Config::load();
    let mut received = crate::server::handoff::receive(socket_path, token)?;
    crate::server::handoff::log_import_result(received.manifest.panes.len());
    let mission_fence = received.manifest.mission_fence;
    let session_data_dir = socket_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "handoff socket has no session data directory",
        )
    })?;
    let mission_runtime = crate::mission::runtime::MissionRuntime::observe_handoff(
        session_data_dir,
        &crate::config::state_dir().join("mission-worktree-locks"),
        mission_fence,
    )
    .map_err(io::Error::other)?;

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();

    let mut imports = HashMap::new();
    for (pane, fd) in received.manifest.panes.into_iter().zip(received.fds) {
        let pane_id = pane.pane_id;
        imports.insert(
            pane_id,
            crate::handoff_runtime::ImportedHandoffRuntime {
                master_fd: fd,
                state: pane,
            },
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        let mut app = app::App::new_from_handoff(
            &loaded_config.config,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub.clone(),
            &received.manifest.snapshot,
            &mut imports,
        )?;
        app.state.local_sound_playback = false;
        app.local_terminal_notifications = false;
        app.local_input_source_switch = false;
        crate::server::handoff::report_restored(&mut received.stream)?;
        if std::env::var("NAGI_TEST_HANDOFF_IMPORT_FAIL").as_deref() == Ok("after_restored") {
            return Err(io::Error::other(
                "test handoff import failure after restored",
            ));
        }
        wait_for_old_public_sockets_to_close(Duration::from_secs(5))?;

        let api_server = api::start_server(api_tx.clone(), event_hub.clone())?;
        let mut server = HeadlessServer::new_with_mission_runtime(
            app,
            &loaded_config.diagnostics,
            Some(api_tx.clone()),
            Some(api_server),
            mission_runtime,
        )?;
        server.acp_endpoint =
            configured_acp_endpoint(&loaded_config.config).unwrap_or_else(|error| {
                warn!(err = %error, "ACP provider configuration is invalid");
                None
            });
        crate::server::handoff::report_ready(&mut received.stream)?;
        crate::server::handoff::wait_committed(&mut received.stream)?;
        server
            .mission_runtime
            .acquire_handoff(mission_fence)
            .map_err(io::Error::other)?;
        crate::server::handoff::report_owned(&mut received.stream)?;
        server.app.assume_handoff_ownership();
        server.app.unpause_handoff_readers();
        server.pending_handoff_repaint_nudge = true;
        info!("handoff import server started");
        print_ready_message(&api::socket_path(), &client_socket_path());
        server.run().await
    });

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

#[cfg(unix)]
fn wait_for_old_public_sockets_to_close(timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let api_socket = api::socket_path();
    let client_socket = client_socket_path();
    while Instant::now() < deadline {
        let api_open = api_socket.exists() && crate::ipc::connect_local_stream(&api_socket).is_ok();
        let client_open =
            client_socket.exists() && crate::ipc::connect_local_stream(&client_socket).is_ok();
        if !api_open && !client_open {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "old server sockets did not close before handoff import bind",
    ))
}

#[cfg(not(unix))]
fn run_handoff_import_server(_socket_path: &Path, _token: &str) -> io::Result<()> {
    Err(io::Error::other("live handoff is only supported on Unix"))
}

fn print_ready_message(api_socket: &Path, client_socket: &Path) {
    eprintln!("nagi server running; you can use any nagi CLI command in another terminal.");
    eprintln!("api socket: {}", api_socket.display());
    eprintln!("client socket: {}", client_socket.display());
    eprintln!(
        "logs: {}",
        crate::session::data_dir().join("nagi-server.log").display()
    );
    eprintln!("did you mean to open the Nagi TUI? run `nagi`; you do not need `nagi server`.");
}

/// Initialize logging for the server process.
fn init_logging() {
    crate::logging::init_file_logging("nagi-server.log");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app::AppState;
    use crate::protocol::CursorState;

    const PROVIDER_FIXTURE_TIMEOUT: Duration = Duration::from_secs(45);

    #[path = "pane_graphics.rs"]
    mod pane_graphics_tests;

    fn test_headless_server() -> HeadlessServer {
        test_headless_server_with_event_hub(api::EventHub::default())
    }

    fn test_headless_server_with_event_hub(event_hub: api::EventHub) -> HeadlessServer {
        static TEST_SERVER_SEQUENCE: AtomicU64 = AtomicU64::new(1);

        let config = crate::config::Config::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(&config, true, None, api_rx, event_hub);
        app.state.local_sound_playback = false;
        app.local_terminal_notifications = false;
        app.local_input_source_switch = false;

        let dir = std::env::temp_dir().join(format!(
            "hh-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            TEST_SERVER_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = fs::create_dir_all(&dir);
        let mission_runtime = crate::mission::runtime::MissionRuntime::open_owned(
            &dir,
            &dir.join("mission-worktree-locks"),
        )
        .expect("open test mission runtime");
        let socket_path = dir.join("client.sock");
        let _ = fs::remove_file(&socket_path);
        let listener = bind_local_listener(&socket_path).expect("bind test listener");
        let client_socket_identity =
            socket_file_identity(&socket_path).expect("test listener socket identity");
        #[cfg(unix)]
        listener
            .set_nonblocking(ListenerNonblockingMode::Accept)
            .expect("set listener nonblocking");
        let (server_event_tx, server_event_rx) = mpsc::channel(64);
        let (provider_event_tx, provider_event_rx) = mpsc::channel(64);
        let (proof_event_tx, proof_event_rx) = mpsc::channel(8);
        #[cfg(windows)]
        let should_quit = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        spawn_windows_client_accept_thread(listener, should_quit.clone(), server_event_tx.clone());
        let server_keybindings = app_keybindings(&app);

        HeadlessServer {
            app,
            mission_runtime,
            managed_runs: HashMap::new(),
            managed_provider_executable: None,
            acp_endpoint: None,
            provider_event_rx,
            provider_event_tx,
            proof_event_rx,
            proof_event_tx,
            pending_proofs: HashMap::new(),
            pending_mission_launches: HashMap::new(),
            pending_project_launches: HashMap::new(),
            #[cfg(unix)]
            api_tx: None,
            api_server: None,
            #[cfg(unix)]
            client_listener: listener,
            client_socket_path: socket_path,
            client_socket_identity,
            clients: HashMap::new(),
            #[cfg(unix)]
            next_client_id: 1,
            foreground_client_id: None,
            server_keybindings,
            server_config_diagnostic: None,
            server_config_diagnostic_without_keybindings: None,
            terminal_attach_owners: HashMap::new(),
            next_activity_stamp: 1,
            effective_size: (MIN_COLS, MIN_ROWS),
            shutting_down: false,
            handoff_in_progress: false,
            #[cfg(unix)]
            pending_handoff_repaint_nudge: false,
            #[cfg(unix)]
            should_quit: Arc::new(AtomicBool::new(false)),
            #[cfg(windows)]
            should_quit,
            server_event_rx,
            server_event_tx,
        }
    }

    fn test_headless_server_with_mission_runtime(
        mission_runtime: crate::mission::runtime::MissionRuntime,
    ) -> HeadlessServer {
        let config = crate::config::Config::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(&config, true, None, api_rx, api::EventHub::default());
        app.state.local_sound_playback = false;
        app.local_terminal_notifications = false;
        app.local_input_source_switch = false;

        let dir = std::env::temp_dir().join(format!(
            "hh-runtime-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::create_dir_all(&dir);
        let socket_path = dir.join("client.sock");
        let listener = bind_local_listener(&socket_path).expect("bind test listener");
        let client_socket_identity =
            socket_file_identity(&socket_path).expect("test listener socket identity");
        #[cfg(unix)]
        listener
            .set_nonblocking(ListenerNonblockingMode::Accept)
            .expect("set listener nonblocking");
        let (server_event_tx, server_event_rx) = mpsc::channel(64);
        let (provider_event_tx, provider_event_rx) = mpsc::channel(64);
        let (proof_event_tx, proof_event_rx) = mpsc::channel(8);
        #[cfg(windows)]
        let should_quit = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        spawn_windows_client_accept_thread(listener, should_quit.clone(), server_event_tx.clone());
        let server_keybindings = app_keybindings(&app);

        HeadlessServer {
            app,
            mission_runtime,
            managed_runs: HashMap::new(),
            managed_provider_executable: None,
            acp_endpoint: None,
            provider_event_rx,
            provider_event_tx,
            proof_event_rx,
            proof_event_tx,
            pending_proofs: HashMap::new(),
            pending_mission_launches: HashMap::new(),
            pending_project_launches: HashMap::new(),
            #[cfg(unix)]
            api_tx: None,
            api_server: None,
            #[cfg(unix)]
            client_listener: listener,
            client_socket_path: socket_path,
            client_socket_identity,
            clients: HashMap::new(),
            #[cfg(unix)]
            next_client_id: 1,
            foreground_client_id: None,
            server_keybindings,
            server_config_diagnostic: None,
            server_config_diagnostic_without_keybindings: None,
            terminal_attach_owners: HashMap::new(),
            next_activity_stamp: 1,
            effective_size: (MIN_COLS, MIN_ROWS),
            shutting_down: false,
            handoff_in_progress: false,
            #[cfg(unix)]
            pending_handoff_repaint_nudge: false,
            #[cfg(unix)]
            should_quit: Arc::new(AtomicBool::new(false)),
            #[cfg(windows)]
            should_quit,
            server_event_rx,
            server_event_tx,
        }
    }

    fn shutdown_test_runtimes(server: &mut HeadlessServer) {
        for (_, runtime) in server.app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    fn configure_test_mission(
        runtime: &mut crate::mission::runtime::MissionRuntime,
        mission_id: &str,
        at_millis: u64,
    ) {
        use crate::mission::{
            evidence::{CheckDeclaration, CommandSpec, PathRule},
            model::MissionDefinition,
            runtime::ConfigureMission,
        };

        let mission = runtime.mission(mission_id).unwrap();
        let criteria = MissionDefinition::criterion_ids(&mission.acceptance_criteria);
        runtime
            .configure_mission(ConfigureMission {
                mission_id: mission_id.to_owned(),
                declarations: vec![CheckDeclaration::command(
                    "test",
                    CommandSpec::new("cargo", ["test"], "."),
                    vec![PathRule::All],
                    Vec::new(),
                )
                .covers(criteria)],
                at_millis,
            })
            .unwrap();
    }

    fn read_server_message(bytes: Vec<u8>) -> ServerMessage {
        let mut cursor = std::io::Cursor::new(bytes);
        protocol::read_message(&mut cursor, MAX_FRAME_SIZE).expect("decode server message")
    }

    fn read_server_frame(bytes: Vec<u8>) -> FrameData {
        match read_server_message(bytes) {
            ServerMessage::Frame(frame) => frame,
            other => panic!("expected frame, got {other:?}"),
        }
    }

    fn frame_text(frame: &FrameData) -> String {
        frame
            .cells
            .chunks(usize::from(frame.width))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn read_server_shutdown_reason(bytes: Vec<u8>) -> Option<String> {
        match read_server_message(bytes) {
            ServerMessage::ServerShutdown { reason } => reason,
            other => panic!("expected shutdown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn headless_api_reads_latest_title_without_spinner_event_flooding() {
        let event_hub = api::EventHub::default();
        let mut server = test_headless_server_with_event_hub(event_hub.clone());
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("one")];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.sidebar_agents.rows = vec![vec![
            crate::config::AgentSidebarToken::TerminalTitleStripped,
        ]];
        let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = server.app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .detected_agent = Some(crate::detect::Agent::Claude);
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes(b"\x1b]0;\xe2\xa0\x8b task\x07");
        server
            .app
            .terminal_runtimes
            .insert(terminal_id.clone(), runtime);

        let first = headless_pane_list(&mut server).pop().unwrap();
        assert_eq!(first.terminal_title.as_deref(), Some("⠋ task"));
        assert_eq!(first.terminal_title_stripped.as_deref(), Some("task"));
        assert_eq!(pane_updated_events(&event_hub), 1);
        let (buffer, _) = crate::server::render_stream::render_virtual_with_runtime_registry(
            &mut server.app.state,
            &server.app.terminal_runtimes,
            Rect::new(0, 0, 100, 30),
            true,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("task"), "rendered frame: {rendered:?}");

        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b]2;\xe2\xa0\x99 task\x1b\\");
        let second = headless_pane_list(&mut server).pop().unwrap();
        assert_eq!(second.terminal_title.as_deref(), Some("⠙ task"));
        assert_eq!(second.terminal_title_stripped.as_deref(), Some("task"));
        assert_eq!(pane_updated_events(&event_hub), 1);
    }

    fn headless_pane_list(server: &mut HeadlessServer) -> Vec<api::schema::PaneInfo> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "list-titles".into(),
                method: api::schema::Method::PaneList(api::schema::PaneListParams::default()),
            },
            respond_to,
            response_write_complete: None,
        });
        let response: api::schema::SuccessResponse =
            serde_json::from_str(&response_rx.recv().unwrap()).unwrap();
        let api::schema::ResponseResult::PaneList { panes } = response.result else {
            panic!("expected pane list");
        };
        panes
    }

    fn pane_updated_events(event_hub: &api::EventHub) -> usize {
        event_hub
            .events_after(0)
            .iter()
            .filter(|(_, event)| event.event == api::schema::EventKind::PaneUpdated)
            .count()
    }

    #[test]
    fn headless_api_request_drains_all_pending_internal_events_before_reading_state() {
        let mut server = test_headless_server();
        for i in 0..=crate::app::APP_EVENT_DRAIN_LIMIT {
            server
                .app
                .event_tx
                .try_send(AppEvent::UpdateReady {
                    version: format!("4.0.{i}"),
                    install_command: "nagi install".into(),
                })
                .unwrap();
        }

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "headless_stop_after_events".into(),
                    method: api::schema::Method::ServerStop(api::schema::EmptyParams::default()),
                },
                respond_to,
                response_write_complete: None,
            })
        );
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["result"]["type"], "ok");
        let expected_version = format!("4.0.{}", crate::app::APP_EVENT_DRAIN_LIMIT);
        assert_eq!(
            server.app.state.update_available.as_deref(),
            Some(expected_version.as_str())
        );
        assert!(server.app.event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn headless_deferred_workspace_create_uses_runtime_events() {
        let event_hub = api::EventHub::default();
        let mut server = test_headless_server_with_event_hub(event_hub.clone());

        server.app.state.request_new_workspace = true;

        assert!(server.handle_deferred_requests_headless());
        assert!(!server.app.state.request_new_workspace);
        assert_eq!(
            event_hub
                .events_after(0)
                .into_iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            vec![
                api::schema::EventKind::WorkspaceCreated,
                api::schema::EventKind::TabCreated,
                api::schema::EventKind::PaneCreated,
                api::schema::EventKind::LayoutUpdated,
            ]
        );
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn headless_deferred_named_tab_create_uses_runtime_events() {
        let event_hub = api::EventHub::default();
        let mut server = test_headless_server_with_event_hub(event_hub.clone());
        server
            .app
            .create_workspace_with_options(std::env::temp_dir(), true)
            .unwrap();
        let after_setup = event_hub.current_sequence();

        server.app.state.request_new_tab = true;
        server.app.state.requested_new_tab_name = Some("ops".into());

        assert!(server.handle_deferred_requests_headless());
        assert!(!server.app.state.request_new_tab);
        assert_eq!(server.app.state.requested_new_tab_name, None);
        let events = event_hub.events_after(after_setup);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            vec![
                api::schema::EventKind::TabCreated,
                api::schema::EventKind::PaneCreated,
                api::schema::EventKind::LayoutUpdated,
            ]
        );
        let tab_created = events
            .iter()
            .find_map(|(_, event)| match &event.data {
                api::schema::EventData::TabCreated { tab } => Some(tab),
                _ => None,
            })
            .expect("tab created event");
        assert_eq!(tab_created.label, "ops");
        shutdown_test_runtimes(&mut server);
    }

    fn test_client_writer() -> (
        ClientWriter,
        std::sync::mpsc::Receiver<Vec<u8>>,
        std::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        let (control_tx, control_rx) = std::sync::mpsc::channel();
        let (render_tx, render_rx) = std::sync::mpsc::sync_channel(1);
        (
            ClientWriter::test_channel(control_tx, render_tx),
            control_rx,
            render_rx,
        )
    }

    fn retained_test_server(
        initial_screen: &[u8],
    ) -> (
        HeadlessServer,
        std::sync::mpsc::Receiver<Vec<u8>>,
        crate::layout::PaneId,
    ) {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.focused_pane_id().expect("focused pane");
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, initial_screen),
        );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (client_tx, _client_control_rx, client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        (server, client_rx, pane_id)
    }

    fn assert_frame_data_eq(actual: &FrameData, expected: &FrameData) {
        assert_eq!(
            (actual.width, actual.height),
            (expected.width, expected.height)
        );
        assert_eq!(actual.cursor, expected.cursor, "cursor mismatch");
        assert_eq!(actual.hyperlinks, expected.hyperlinks, "hyperlink mismatch");
        assert_eq!(actual.graphics, expected.graphics, "graphics mismatch");
        assert_eq!(
            actual.cells.len(),
            expected.cells.len(),
            "cell length mismatch"
        );
        for (idx, (actual_cell, expected_cell)) in
            actual.cells.iter().zip(expected.cells.iter()).enumerate()
        {
            assert_eq!(
                actual_cell,
                expected_cell,
                "cell mismatch at index {idx} (x={}, y={})",
                idx % usize::from(actual.width),
                idx / usize::from(actual.width),
            );
        }
    }

    #[test]
    fn foreground_client_applies_client_keybindings() {
        let mut server = test_headless_server();
        let local_config: crate::config::Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_tab = "prefix+t"
"#,
        )
        .unwrap();
        let local_keybindings = local_config.live_keybinds().unwrap();
        let (writer_a, _control_a, _render_a) = test_client_writer();
        let (writer_b, _control_b, _render_b) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: Some(Box::new(local_keybindings)),
            direct_attach_requested: false,
            writer: writer_a,
        }));
        assert_eq!(
            server.app.state.prefix_code,
            crossterm::event::KeyCode::Char('a')
        );
        assert!(server
            .app
            .state
            .keybinds
            .new_tab
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+t"));

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            writer: writer_b,
        }));
        assert_eq!(
            server.app.state.prefix_code,
            crossterm::event::KeyCode::Char('b')
        );
        assert!(server
            .app
            .state
            .keybinds
            .new_tab
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+c"));
    }

    #[test]
    fn local_keybinding_client_hides_server_keybinding_warnings() {
        let mut server = test_headless_server();
        let diagnostics = vec![
            "unsafe direct keybinding: keys.close_pane = \"x\" would intercept typing".to_owned(),
            "theme warning".to_owned(),
        ];
        let (full, without_keybindings) = server_config_diagnostic_summaries(&diagnostics);
        server.server_config_diagnostic = full.clone();
        server.server_config_diagnostic_without_keybindings = without_keybindings.clone();
        server.app.state.config_diagnostic = full;
        let local_keybindings = crate::config::Config::default().live_keybinds().unwrap();
        let (writer_a, _control_a, _render_a) = test_client_writer();
        let (writer_b, _control_b, _render_b) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: Some(Box::new(local_keybindings)),
            direct_attach_requested: false,
            writer: writer_a,
        }));
        assert_eq!(server.app.state.config_diagnostic, without_keybindings);

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            writer: writer_b,
        }));
        assert_eq!(
            server.app.state.config_diagnostic,
            server.server_config_diagnostic
        );
    }

    #[test]
    fn local_keybinding_client_keeps_local_keybindings_after_settings_save() {
        let path = std::env::temp_dir().join(format!(
            "nagi-headless-settings-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, "onboarding = false\n").unwrap();
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

        let mut server = test_headless_server();
        let local_config: crate::config::Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_workspace = "prefix+n"
next_tab = ""
"#,
        )
        .unwrap();
        let local_keybindings = local_config.live_keybinds().unwrap();
        let (writer, _control, _render) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: Some(Box::new(local_keybindings)),
            direct_attach_requested: false,
            writer,
        }));
        server.app.state.mode = crate::app::Mode::Settings;
        server.app.state.settings.section = crate::app::state::SettingsSection::Toast;
        server.app.state.settings.list.selected = 1;

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\r".to_vec(),
        }));

        assert_eq!(
            server.app.state.prefix_code,
            crossterm::event::KeyCode::Char('a')
        );
        assert!(server
            .app
            .state
            .keybinds
            .new_workspace
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+n"));
        assert!(server.app.state.toast.is_none());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("delivery = \"nagi\""));

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_server_keybindings_apply_valid_subset_after_settings_save_without_caching_local_keybindings(
    ) {
        let path = std::env::temp_dir().join(format!(
            "nagi-headless-invalid-settings-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(
            &path,
            "onboarding = false\n[keys]\nnew_workspace = \"x\"\n[ui.toast]\ndelivery = \"off\"\n",
        )
        .unwrap();
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

        let mut server = test_headless_server();
        let previous_server_config: crate::config::Config =
            toml::from_str("[keys]\nprefix = \"ctrl+c\"\nnew_workspace = \"prefix+m\"\n").unwrap();
        server.server_keybindings = previous_server_config.live_keybinds().unwrap();
        let local_config: crate::config::Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_workspace = "prefix+n"
next_tab = ""
"#,
        )
        .unwrap();
        let (writer_a, _control_a, _render_a) = test_client_writer();
        let (writer_b, _control_b, _render_b) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: Some(Box::new(local_config.live_keybinds().unwrap())),
            direct_attach_requested: false,
            writer: writer_a,
        }));
        server.app.state.mode = crate::app::Mode::Settings;
        server.app.state.settings.section = crate::app::state::SettingsSection::Toast;
        server.app.state.settings.list.selected = 1;

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\r".to_vec(),
        }));

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            writer: writer_b,
        }));
        assert_eq!(
            server.app.state.prefix_code,
            crossterm::event::KeyCode::Char('b')
        );
        assert!(!server
            .app
            .state
            .keybinds
            .new_workspace
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+n"));
        assert!(server.app.state.keybinds.new_workspace.bindings.is_empty());

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn terminal_attach_rejects_missing_terminal_and_removes_client() {
        let mut server = test_headless_server();
        let (writer, control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            writer,
        }));
        assert!(server.clients.contains_key(&7));

        assert!(
            !server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: "term_missing".to_owned(),
                takeover: false,
            })
        );
        assert!(!server.clients.contains_key(&7));
        let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
        assert_eq!(
            reason,
            Some("terminal attach failed: terminal term_missing not found".to_owned())
        );
    }

    fn with_terminal_session_test_server(
        test: impl FnOnce(&mut HeadlessServer, crate::terminal::TerminalId, String, String),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        let terminal_id_string = terminal_id.to_string();
        let public_pane_id = format!("{}:p1", workspace.id);
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );

        test(&mut server, terminal_id, terminal_id_string, public_pane_id);

        drop(server);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    fn connect_pending_terminal_client(server: &mut HeadlessServer, client_id: u64) {
        let _control_rx = connect_pending_terminal_client_with_control_rx(server, client_id);
    }

    fn connect_pending_terminal_client_with_control_rx(
        server: &mut HeadlessServer,
        client_id: u64,
    ) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (writer, control_rx, _render_rx) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id,
            cols: 100,
            rows: 30,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            writer,
        }));
        control_rx
    }

    #[test]
    fn terminal_observe_allows_multiple_clients_without_attach_ownership() {
        with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
            let initial_size = server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("runtime")
                .current_size();

            for client_id in [7, 8] {
                connect_pending_terminal_client(server, client_id);
                assert!(
                    server.handle_server_event(ServerEvent::ClientObserveTerminal {
                        client_id,
                        target: terminal_id_string.clone(),
                    })
                );
            }

            assert!(server.terminal_attach_owners.is_empty());
            assert!(!server
                .app
                .state
                .direct_attach_resize_locks
                .contains(&terminal_id));
            assert_eq!(
                server
                    .app
                    .terminal_runtimes
                    .get(&terminal_id)
                    .expect("runtime")
                    .current_size(),
                initial_size
            );
            assert_eq!(
                terminal_stream_client_ids(&server.clients, &terminal_id_string).len(),
                2
            );
        });
    }

    #[test]
    fn terminal_observe_resolves_public_pane_id() {
        with_terminal_session_test_server(|server, terminal_id, _, public_pane_id| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id: 7,
                    target: public_pane_id,
                })
            );

            assert!(matches!(
                server.clients.get(&7).map(|client| &client.mode),
                Some(ClientConnectionMode::TerminalObserve { terminal_id: observed })
                    if observed == &terminal_id.to_string()
            ));
        });
    }

    #[test]
    fn terminal_control_resolves_public_pane_id_and_takes_ownership() {
        with_terminal_session_test_server(
            |server, terminal_id, terminal_id_string, public_pane_id| {
                connect_pending_terminal_client(server, 7);
                assert!(
                    server.handle_server_event(ServerEvent::ClientControlTerminal {
                        client_id: 7,
                        target: public_pane_id,
                        takeover: false,
                    })
                );

                assert!(matches!(
                    server.clients.get(&7).map(|client| &client.mode),
                    Some(ClientConnectionMode::TerminalAttach { terminal_id: attached })
                        if attached == &terminal_id_string
                ));
                assert_eq!(
                    server.terminal_attach_owners.get(&terminal_id_string),
                    Some(&7)
                );
                assert!(server
                    .app
                    .state
                    .direct_attach_resize_locks
                    .contains(&terminal_id));
            },
        );
    }

    #[test]
    fn terminal_control_rejects_second_controller_without_takeover() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            connect_pending_terminal_client(server, 8);
            assert!(
                !server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 8,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            assert!(server.clients.contains_key(&7));
            assert!(!server.clients.contains_key(&8));
            assert_eq!(
                server.terminal_attach_owners.get(&terminal_id_string),
                Some(&7)
            );
        });
    }

    #[test]
    fn terminal_control_takeover_replaces_existing_controller() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            connect_pending_terminal_client(server, 8);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 8,
                    target: terminal_id_string.clone(),
                    takeover: true,
                })
            );

            assert!(!server.clients.contains_key(&7));
            assert!(server.clients.contains_key(&8));
            assert_eq!(
                server.terminal_attach_owners.get(&terminal_id_string),
                Some(&8)
            );
        });
    }

    #[test]
    fn terminal_observe_can_coexist_with_terminal_control() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            connect_pending_terminal_client(server, 8);
            assert!(
                server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id: 8,
                    target: terminal_id_string.clone(),
                })
            );

            assert_eq!(
                server.terminal_attach_owners.get(&terminal_id_string),
                Some(&7)
            );
            assert!(matches!(
                server.clients.get(&8).map(|client| &client.mode),
                Some(ClientConnectionMode::TerminalObserve { terminal_id })
                    if terminal_id == &terminal_id_string
            ));
            assert_eq!(
                terminal_stream_client_ids(&server.clients, &terminal_id_string).len(),
                2
            );
        });
    }

    #[test]
    fn terminal_control_detach_sends_shutdown_before_removal() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
            let control_rx = connect_pending_terminal_client_with_control_rx(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            assert!(server.handle_server_event(ServerEvent::ClientDetach { client_id: 7 }));

            assert!(!server.clients.contains_key(&7));
            assert!(!server
                .terminal_attach_owners
                .contains_key(&terminal_id_string));
            let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
            assert_eq!(reason, Some("detached".to_owned()));
        });
    }

    #[test]
    fn terminal_observe_rejects_later_attach_upgrade() {
        with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                })
            );
            assert!(
                !server.handle_server_event(ServerEvent::ClientAttachTerminal {
                    client_id: 7,
                    terminal_id: terminal_id_string,
                    takeover: true,
                })
            );

            assert!(!server.clients.contains_key(&7));
            assert!(server.terminal_attach_owners.is_empty());
            assert!(!server
                .app
                .state
                .direct_attach_resize_locks
                .contains(&terminal_id));
        });
    }

    #[test]
    fn terminal_attach_rejects_later_observe_and_clears_ownership() {
        with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientAttachTerminal {
                    client_id: 7,
                    terminal_id: terminal_id_string.clone(),
                    takeover: false,
                })
            );
            assert_eq!(
                server.terminal_attach_owners.get(&terminal_id_string),
                Some(&7)
            );
            assert!(server
                .app
                .state
                .direct_attach_resize_locks
                .contains(&terminal_id));

            assert!(
                !server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                })
            );

            assert!(!server.clients.contains_key(&7));
            assert!(server.terminal_attach_owners.is_empty());
            assert!(!server
                .app
                .state
                .direct_attach_resize_locks
                .contains(&terminal_id));
        });
    }

    fn app_client_marks_git_refresh_due_on_first_attach(render_encoding: RenderEncoding) {
        let mut server = test_headless_server();
        server
            .app
            .state
            .workspaces
            .push(crate::workspace::Workspace::test_new("test"));
        let future = Instant::now() + Duration::from_secs(60);
        server.app.last_git_remote_status_refresh = future;
        let (writer, _control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding,
            keybindings: None,
            direct_attach_requested: false,
            writer,
        }));

        assert!(server.has_app_client());
        assert!(server
            .app
            .git_refresh_deadline()
            .is_some_and(|deadline| deadline <= Instant::now()));
    }

    #[test]
    fn terminal_ansi_app_client_enables_headless_git_refresh() {
        app_client_marks_git_refresh_due_on_first_attach(RenderEncoding::TerminalAnsi);
    }

    #[test]
    fn pending_terminal_attach_client_does_not_enable_headless_git_refresh() {
        let mut server = test_headless_server();
        server
            .app
            .state
            .workspaces
            .push(crate::workspace::Workspace::test_new("test"));
        let (writer, _control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            writer,
        }));

        assert!(!server.has_app_client());
        assert_eq!(
            server.app.next_headless_loop_deadline_with_git_refresh(
                Instant::now(),
                false,
                server.has_app_client()
            ),
            None
        );
    }

    #[test]
    fn writerless_app_client_does_not_enable_headless_git_refresh() {
        let mut server = test_headless_server();
        server
            .app
            .state
            .workspaces
            .push(crate::workspace::Workspace::test_new("test"));
        let (writer, _control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            writer,
        }));
        assert!(server.has_app_client());

        server.clients.get_mut(&7).expect("client").writer = None;

        assert!(!server.has_app_client());
        assert_eq!(
            server.app.next_headless_loop_deadline_with_git_refresh(
                Instant::now(),
                false,
                server.has_app_client()
            ),
            None
        );
    }

    #[test]
    fn semantic_app_client_marks_git_refresh_due_on_first_attach() {
        app_client_marks_git_refresh_due_on_first_attach(RenderEncoding::SemanticFrame);
    }

    #[test]
    fn terminal_attach_client_exits_when_attached_pane_dies() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("attached");
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        let terminal_id = server.app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane")
            .attached_terminal_id
            .to_string();
        let (writer, control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            writer,
        }));
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id.clone(),
                takeover: false,
            })
        );
        assert_eq!(server.terminal_attach_owners.get(&terminal_id), Some(&7));

        assert!(server.handle_internal_event_with_forwarding(AppEvent::PaneDied { pane_id }));

        assert!(!server.clients.contains_key(&7));
        assert!(!server.terminal_attach_owners.contains_key(&terminal_id));
        let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
        assert_eq!(reason, Some(format!("terminal {terminal_id} exited")));
    }

    #[test]
    fn terminal_attach_scroll_moves_attached_runtime_viewport() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);

        apply_terminal_attach_scroll(
            &runtime,
            AttachScrollSource::Wheel,
            AttachScrollDirection::Up,
            3,
            None,
            None,
            0,
        )
        .expect("scroll up");
        let metrics = runtime.scroll_metrics().expect("scroll metrics");
        assert_eq!(metrics.offset_from_bottom, 3);

        apply_terminal_attach_scroll(
            &runtime,
            AttachScrollSource::Wheel,
            AttachScrollDirection::Down,
            2,
            None,
            None,
            0,
        )
        .expect("scroll down");
        let metrics = runtime.scroll_metrics().expect("scroll metrics");
        assert_eq!(metrics.offset_from_bottom, 1);
        drop(runtime);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    #[test]
    fn terminal_attach_input_resets_scrolled_viewport() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                20, 5, 4096, &bytes, 4,
            );

        runtime.scroll_up(4);
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            4
        );

        apply_terminal_attach_input(&runtime, b"x".to_vec()).expect("attach input");
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input_rx.try_recv().expect("forwarded input"),
            Bytes::from("x")
        );

        drop(runtime);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    fn with_terminal_attach_page_key_runtime(
        initial_bytes: &[u8],
        initial_scroll: usize,
        test: impl FnOnce(&crate::terminal::TerminalRuntime, &mut mpsc::Receiver<Bytes>),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut bytes = initial_bytes.to_vec();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                20, 5, 4096, &bytes, 4,
            );
        if initial_scroll > 0 {
            runtime.scroll_up(initial_scroll);
        }

        test(&runtime, &mut input_rx);

        drop(runtime);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    fn apply_terminal_attach_page_up(runtime: &crate::terminal::TerminalRuntime) {
        apply_terminal_attach_scroll(
            runtime,
            AttachScrollSource::PageKey {
                input: b"\x1b[5~".to_vec(),
            },
            AttachScrollDirection::Up,
            4,
            None,
            None,
            0,
        )
        .expect("page key");
    }

    #[test]
    fn terminal_attach_page_key_host_scrolls_plain_terminal() {
        with_terminal_attach_page_key_runtime(b"", 0, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                4
            );
            assert!(input_rx.try_recv().is_err());
        });
    }

    #[test]
    fn terminal_attach_page_key_forwards_when_mouse_reporting() {
        with_terminal_attach_page_key_runtime(b"\x1b[?1000h", 3, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                0
            );
            assert_eq!(
                input_rx.try_recv().expect("forwarded page key"),
                Bytes::from_static(b"\x1b[5~")
            );
        });
    }

    #[test]
    fn terminal_attach_page_key_forwards_when_application_cursor() {
        with_terminal_attach_page_key_runtime(b"\x1b[?1h", 3, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                0
            );
            assert_eq!(
                input_rx.try_recv().expect("forwarded page key"),
                Bytes::from_static(b"\x1b[5~")
            );
        });
    }

    #[test]
    fn terminal_attach_page_key_forwards_in_alternate_screen_without_mouse_reporting() {
        with_terminal_attach_page_key_runtime(b"\x1b[?1049h", 3, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                0
            );
            assert_eq!(
                input_rx.try_recv().expect("forwarded page key"),
                Bytes::from_static(b"\x1b[5~")
            );
        });
    }

    #[test]
    fn headless_scheduled_tasks_expire_agent_metadata() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("metadata");
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::HookStateReported {
                pane_id,
                source: "nagi:pi".into(),
                agent_label: "pi".into(),
                state: crate::detect::AgentState::Working,
                message: None,
                seq: None,
                session_ref: None,
            })
        );
        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::HookMetadataReported {
                pane_id,
                source: "user:pi-display".into(),
                agent_label: Some("pi".into()),
                applies_to_source: Some("nagi:pi".into()),
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
                clear_title: false,
                clear_display_agent: false,
                clear_state_labels: false,
                seq: None,
                ttl: Some(Duration::from_millis(1)),
            })
        );

        let deadline = server
            .app
            .agent_metadata_deadline
            .expect("metadata deadline");
        let terminal_id = server.app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane")
            .attached_terminal_id
            .clone();
        assert_eq!(
            server
                .app
                .state
                .terminals
                .get(&terminal_id)
                .expect("terminal")
                .effective_title()
                .as_deref(),
            Some("short lived")
        );

        assert!(server.handle_scheduled_tasks_headless(deadline + Duration::from_millis(1), false));

        assert_eq!(server.app.agent_metadata_deadline, None);
        assert_eq!(
            server
                .app
                .state
                .terminals
                .get(&terminal_id)
                .expect("terminal")
                .effective_title(),
            None
        );
        assert!(server
            .app
            .event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| {
                event.event == crate::api::schema::EventKind::PaneAgentStatusChanged
                    && matches!(
                        &event.data,
                        crate::api::schema::EventData::PaneAgentStatusChanged {
                            title,
                            ..
                        } if title.is_none()
                    )
            }));
    }

    #[test]
    fn headless_scheduled_tasks_clears_disabled_agent_manifest_update_deadline() {
        let mut server = test_headless_server();
        let now = Instant::now();
        server.app.next_agent_manifest_update_check = Some(now - Duration::from_millis(1));

        assert!(!server.handle_scheduled_tasks_headless(now, false));
        assert_eq!(server.app.next_agent_manifest_update_check, None);
    }

    #[tokio::test]
    async fn headless_scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        server.app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.ensure_test_terminals();
        server.clients.insert(
            1,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                server.app.state.host_terminal_theme,
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.effective_size = (100, 30);
        server.app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "nagi:codex\0codex\0Id\0codex-session".into(),
        });
        server.app.pending_agent_resume_deadline = Some(Instant::now() - Duration::from_millis(1));

        assert!(!server.handle_scheduled_tasks_headless(Instant::now(), true));
        assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
        assert!(server.app.pending_agent_resume_deadline.is_none());
    }

    #[tokio::test]
    async fn headless_scheduled_tasks_do_not_start_pending_agent_resume_without_foreground_client()
    {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        server.app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 80, 24));
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.ensure_test_terminals();
        server.foreground_client_id = None;
        server.effective_size = (80, 24);
        server.app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "nagi:codex\0codex\0Id\0codex-session".into(),
        });
        server.app.pending_agent_resume_deadline = Some(Instant::now() - Duration::from_millis(1));

        assert!(!server.handle_scheduled_tasks_headless(Instant::now(), false));
        assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
        assert!(server.app.pending_agent_resume_deadline.is_none());
    }

    #[tokio::test]
    async fn headless_pre_input_resize_does_not_start_pending_agent_resume() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        server.app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.ensure_test_terminals();
        server.clients.insert(
            1,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                server.app.state.host_terminal_theme,
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.effective_size = (100, 30);
        server.app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "nagi:codex\0codex\0Id\0codex-session".into(),
        });
        server.app.pending_agent_resume_deadline = Some(Instant::now() - Duration::from_millis(1));

        server.resize_shared_runtime_to_effective_size_before_input();

        assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
        assert!(server.app.pending_agent_resume_deadline.is_none());
    }

    #[test]
    fn virtual_render_produces_nonempty_buffer() {
        let mut state = AppState::test_new();
        let area = Rect::new(0, 0, 80, 24);
        let (buffer, _cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        assert_eq!(buffer.area.width, 80);
        assert_eq!(buffer.area.height, 24);
    }

    #[test]
    fn virtual_render_without_frame_cursor_keeps_cursor_hidden() {
        let mut state = AppState::test_new();
        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert_eq!(cursor, None);
    }

    #[tokio::test]
    async fn virtual_render_preserves_explicit_frame_cursor_position() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        let pane = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .expect("focused pane info");

        assert_eq!(
            cursor,
            Some(CursorState {
                x: pane.inner_rect.x + 4,
                y: pane.inner_rect.y,
                visible: true,
                shape: cursor.as_ref().map(|c| c.shape).unwrap_or(0),
            })
        );
    }

    #[tokio::test]
    async fn virtual_render_preserves_hidden_focused_pane_cursor_position() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        let pane = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .expect("focused pane info");

        assert_eq!(
            cursor,
            Some(CursorState {
                x: pane.inner_rect.x + 4,
                y: pane.inner_rect.y,
                visible: false,
                shape: cursor.as_ref().map(|c| c.shape).unwrap_or(0),
            })
        );
    }

    #[tokio::test]
    async fn virtual_render_hides_focused_pane_cursor_during_synchronized_output() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left");
        ws.insert_test_runtime(pane_id, runtime);

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let runtime = state
            .runtime_for_pane(&terminal_runtimes, pane_id)
            .expect("pane runtime after initial render");
        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[2;3H");
        assert!(runtime.synchronized_output_active());

        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, false);

        assert_eq!(
            cursor, None,
            "child cursor positions are unstable while synchronized output is active"
        );
    }

    #[tokio::test]
    async fn virtual_render_hides_focused_pane_cursor_during_synchronized_output_resize() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left");
        ws.insert_test_runtime(pane_id, runtime);

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let initial_area = Rect::new(0, 0, 80, 24);
        let _ = crate::server::render_stream::render_virtual(&mut state, initial_area, true);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let runtime = state
            .runtime_for_pane(&terminal_runtimes, pane_id)
            .expect("pane runtime after initial render");
        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[2;3H");
        assert!(runtime.synchronized_output_active());

        let resized_area = Rect::new(0, 0, 100, 30);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, resized_area, true);

        assert_eq!(
            cursor, None,
            "pre-resize synchronized output should suppress the cursor even if resize clears the mode"
        );
    }

    #[tokio::test]
    async fn virtual_render_exposes_hidden_pane_cursor_when_reveal_hidden_for_cjk_ime() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        let pane = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .expect("focused pane info");

        assert_eq!(
            cursor,
            Some(CursorState {
                x: pane.inner_rect.x + 4,
                y: pane.inner_rect.y,
                visible: true,
                shape: state.cjk_ime_cursor_shape,
            })
        );
    }

    #[tokio::test]
    async fn virtual_render_keeps_cursor_hidden_when_scrolled_back_even_with_reveal_hidden_for_cjk_ime(
    ) {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);
        ws.insert_test_runtime(pane_id, runtime);

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let runtime = state
            .runtime_for_pane(&terminal_runtimes, pane_id)
            .expect("pane runtime after initial render");
        runtime.scroll_up(6);
        assert!(crate::ui::pane_is_scrolled_back(runtime));

        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert!(
            cursor.as_ref().is_none_or(|cursor| !cursor.visible),
            "scrolled-back focused pane should keep the cursor hidden even when reveal_hidden_cursor_for_cjk_ime is true; got {cursor:?}",
        );
    }

    #[tokio::test]
    async fn virtual_render_fallback_cursor_when_viewport_none_and_reveal_hidden_for_cjk_ime() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        // Feed only ?25l with no prior cursor movement — exercises the fallback
        // path for TUIs whose viewport has no cursor position.
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        let pane = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .expect("focused pane info");

        assert_eq!(
            cursor,
            Some(CursorState {
                x: pane.inner_rect.x,
                y: pane.inner_rect.y,
                visible: true,
                shape: state.cjk_ime_cursor_shape,
            }),
            "fallback should anchor at pane top-left with the configured shape",
        );
    }

    #[tokio::test]
    async fn virtual_render_skips_reveal_when_focused_pane_has_no_detected_agent() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        // Filter only Claude, but the test pane has no detected agent, so the
        // reveal must not apply.
        state.cjk_ime_agent_filter_configured = true;
        state.cjk_ime_agents = vec![crate::detect::Agent::Claude];
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert!(
            cursor.as_ref().is_none_or(|cursor| !cursor.visible),
            "agent filter should suppress reveal when the focused pane's detected agent is not on the list; got {cursor:?}",
        );
    }

    #[tokio::test]
    async fn virtual_render_skips_reveal_when_agent_filter_has_no_valid_entries() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        state.cjk_ime_agent_filter_configured = true;
        state.cjk_ime_agents = Vec::new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert!(
            cursor.as_ref().is_none_or(|cursor| !cursor.visible),
            "agent filter with no valid entries should suppress reveal; got {cursor:?}",
        );
    }

    #[tokio::test]
    async fn virtual_render_omits_focused_pane_cursor_while_mobile_switcher_open() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Navigate;

        let area = Rect::new(0, 0, 44, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert_eq!(cursor, None);
    }

    #[tokio::test]
    async fn virtual_render_hides_focused_pane_cursor_while_scrolled_back() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);
        ws.insert_test_runtime(pane_id, runtime);

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let runtime = state
            .runtime_for_pane(&terminal_runtimes, pane_id)
            .expect("pane runtime after initial render");
        runtime.scroll_up(6);
        assert!(crate::ui::pane_is_scrolled_back(runtime));

        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert!(
            cursor.as_ref().is_none_or(|cursor| !cursor.visible),
            "cursor: {cursor:?}"
        );
    }

    #[test]
    fn latest_active_client_drives_shared_size_theme_and_fallback() {
        let mut server = test_headless_server();

        server.clients.insert(
            1,
            ClientConnection::new(
                (160, 45),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme {
                    foreground: Some(crate::terminal_theme::RgbColor {
                        r: 0xaa,
                        g: 0xbb,
                        b: 0xcc,
                    }),
                    background: Some(crate::terminal_theme::RgbColor {
                        r: 0x11,
                        g: 0x22,
                        b: 0x33,
                    }),
                },
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme {
                    foreground: Some(crate::terminal_theme::RgbColor {
                        r: 0x10,
                        g: 0x20,
                        b: 0x30,
                    }),
                    background: Some(crate::terminal_theme::RgbColor {
                        r: 0xdd,
                        g: 0xee,
                        b: 0xff,
                    }),
                },
                None,
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );

        assert!(server.promote_client_to_foreground(1));
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.effective_size, (160, 45));
        assert_eq!(
            server.app.state.host_terminal_theme,
            server.clients[&1].host_terminal_theme
        );

        assert!(server.promote_client_to_foreground(2));
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.effective_size, (80, 24));
        assert_eq!(
            server.app.state.host_terminal_theme,
            server.clients[&2].host_terminal_theme
        );

        assert!(server.remove_client(2));
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.effective_size, (160, 45));
        assert_eq!(
            server.app.state.host_terminal_theme,
            server.clients[&1].host_terminal_theme
        );
    }

    #[test]
    fn foreground_client_without_host_theme_clears_previous_host_theme() {
        let mut server = test_headless_server();
        let known_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 0x10,
                g: 0x20,
                b: 0x30,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 0x40,
                g: 0x50,
                b: 0x60,
            }),
        };
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                known_theme,
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );

        assert!(server.promote_client_to_foreground(1));
        assert_eq!(server.app.state.host_terminal_theme, known_theme);

        assert!(server.promote_client_to_foreground(2));
        assert_eq!(
            server.app.state.host_terminal_theme,
            crate::terminal_theme::TerminalTheme::default()
        );
    }

    #[test]
    fn foreground_client_appearance_controls_auto_theme() {
        let mut server = test_headless_server();
        server.app.state.theme_runtime.auto_switch = true;
        server.app.state.theme_runtime.dark_name = "catppuccin".to_string();
        server.app.state.theme_runtime.light_name = "catppuccin-latte".to_string();
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme {
                    foreground: None,
                    background: Some(crate::terminal_theme::RgbColor { r: 0, g: 0, b: 0 }),
                },
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme {
                    foreground: None,
                    background: Some(crate::terminal_theme::RgbColor {
                        r: 255,
                        g: 255,
                        b: 255,
                    }),
                },
                None,
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );

        assert!(server.promote_client_to_foreground(1));
        assert_eq!(server.app.state.theme_name, "catppuccin");

        assert!(server.promote_client_to_foreground(2));
        assert_eq!(server.app.state.theme_name, "catppuccin-latte");
    }

    #[test]
    fn color_scheme_change_event_is_inert_on_server() {
        let mut server = test_headless_server();
        let initial_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 0x10,
                g: 0x20,
                b: 0x30,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 0x40,
                g: 0x50,
                b: 0x60,
            }),
        };
        server.app.state.host_terminal_theme = initial_theme;
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                initial_theme,
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );

        let changed = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: crate::raw_input::GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec(),
        });

        assert!(!changed);
        assert_eq!(server.foreground_client_id, None);
        assert_eq!(server.clients[&1].host_terminal_theme, initial_theme);
        assert_eq!(server.app.state.host_terminal_theme, initial_theme);
    }

    #[test]
    fn focus_lost_updates_client_without_promoting_foreground() {
        let mut server = test_headless_server();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        let changed = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[O".to_vec(),
        });

        assert!(!changed);
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(false));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
    }

    #[test]
    fn focus_gained_promotes_client_to_foreground() {
        let mut server = test_headless_server();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        let changed = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        });

        assert!(changed);
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
    }

    #[tokio::test]
    async fn foreground_focus_gained_reaches_pane_with_focus_reporting() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");

        server.clients.insert(1, test_app_client(Some(false), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded focus gained report"),
            Bytes::from_static(b"\x1b[I")
        );

        assert!(!server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[O".to_vec(),
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded focus lost report"),
            Bytes::from_static(b"\x1b[O")
        );
    }

    #[tokio::test]
    async fn outer_focus_events_do_not_reach_pane_without_focus_reporting() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"");
        server.clients.insert(1, test_app_client(Some(false), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn background_focus_batch_only_forwards_events_after_promotion() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.clients.insert(2, test_app_client(Some(false), 2));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 2,
            data: b"\x1b[O\x1b[I".to_vec(),
        }));
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
        assert_eq!(
            input_rx
                .try_recv()
                .expect("focus gained after client promotion"),
            Bytes::from_static(b"\x1b[I")
        );
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn structured_outer_focus_events_reach_reporting_pane() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![
                crate::protocol::ClientInputEvent::FocusGained,
                crate::protocol::ClientInputEvent::FocusLost,
            ],
        }));
        assert_eq!(
            input_rx.try_recv().expect("structured focus gained report"),
            Bytes::from_static(b"\x1b[I")
        );
        assert_eq!(
            input_rx.try_recv().expect("structured focus lost report"),
            Bytes::from_static(b"\x1b[O")
        );
    }

    #[tokio::test]
    async fn background_key_makes_later_focus_lost_eligible() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.clients.insert(2, test_app_client(Some(true), 2));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 2,
            events: vec![
                crate::protocol::ClientInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('x'),
                    modifiers: 0,
                    kind: crate::protocol::ClientKeyKind::Release,
                },
                crate::protocol::ClientInputEvent::FocusLost,
            ],
        }));
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(
            input_rx.try_recv().expect("focus lost after promotion"),
            Bytes::from_static(b"\x1b[O")
        );
    }

    #[tokio::test]
    async fn structured_non_app_focus_is_ignored_without_suppressing_keys() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
        server.clients.insert(1, test_app_client(Some(true), 1));

        let mut attached = test_app_client(Some(false), 2);
        attached.mode = ClientConnectionMode::TerminalAttach {
            terminal_id: "attached".to_owned(),
        };
        server.clients.insert(2, attached);

        let mut pending = test_app_client(Some(false), 3);
        pending.pending_terminal_attach = true;
        server.clients.insert(3, pending);
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        for client_id in [2, 3] {
            assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
                client_id,
                events: vec![crate::protocol::ClientInputEvent::FocusGained],
            }));
            assert_eq!(server.foreground_client_id, Some(1));
            assert_eq!(server.app.state.outer_terminal_focus, Some(true));
            assert_eq!(server.clients[&client_id].outer_terminal_focus, Some(false));
        }

        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 3,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('x'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Release,
            }],
        }));
        assert_eq!(server.foreground_client_id, Some(3));
    }

    fn install_focused_test_runtime(
        server: &mut HeadlessServer,
        terminal_bytes: &[u8],
    ) -> tokio::sync::mpsc::Receiver<Bytes> {
        let mut workspace = crate::workspace::Workspace::test_new("focus-reporting");
        let pane_id = workspace.tabs[0].root_pane;
        let (runtime, input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                terminal_bytes,
                4,
            );
        workspace.insert_test_runtime(pane_id, runtime);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        input_rx
    }

    fn test_app_client(outer_terminal_focus: Option<bool>, last_activity: u64) -> ClientConnection {
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            outer_terminal_focus,
            last_activity,
            RenderEncoding::SemanticFrame,
            None,
        )
    }

    #[test]
    fn foreground_client_focus_event_updates_app_focus_state() {
        let mut server = test_headless_server();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        let changed = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[O".to_vec(),
        });

        assert!(!changed);
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(false));
        assert_eq!(server.app.state.outer_terminal_focus, Some(false));
    }

    #[test]
    fn app_client_lone_escape_closes_navigate_mode() {
        let mut server = test_headless_server();
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Navigate;
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b".to_vec(),
        }));

        assert_eq!(server.app.state.mode, crate::app::Mode::Terminal);
    }

    #[test]
    fn semantic_client_input_events_route_through_app_input() {
        let mut server = test_headless_server();
        server.app.state.mode = crate::app::Mode::Onboarding;
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Enter,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
            }],
        }));

        assert_eq!(server.app.state.mode, crate::app::Mode::NewMission);
        assert!(server.app.state.new_mission.is_some());
    }

    #[tokio::test]
    async fn startup_workspace_preserves_first_run_onboarding() {
        let mut server = test_headless_server();
        assert_eq!(server.app.state.mode, crate::app::Mode::Onboarding);

        seed_startup_workspace(&mut server.app, std::env::temp_dir());

        assert_eq!(server.app.state.mode, crate::app::Mode::Onboarding);
        assert_eq!(server.app.state.workspaces.len(), 1);
    }

    #[test]
    fn semantic_client_escape_closes_keybind_help() {
        let mut server = test_headless_server();
        server.app.state.mode = crate::app::Mode::KeybindHelp;
        server.clients.insert(
            1,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Esc,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
            }],
        }));

        assert_eq!(server.app.state.mode, crate::app::Mode::Navigate);
    }

    #[test]
    fn semantic_client_down_scrolls_keybind_help() {
        let mut server = test_headless_server();
        server.app.state.mode = crate::app::Mode::KeybindHelp;
        server.clients.insert(
            1,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        assert!(server.app.state.keybind_help_max_scroll() > 0);
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Down,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
            }],
        }));

        assert_eq!(server.app.state.mode, crate::app::Mode::KeybindHelp);
        assert_eq!(server.app.state.keybind_help.scroll, 1);
    }

    #[tokio::test]
    async fn split_default_background_response_updates_theme_without_forwarding_tail() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let focused = workspace.focused_pane_id().unwrap();
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        workspace.tabs[0].runtimes.insert(focused, runtime);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        let _ = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b]".to_vec(),
        });
        assert!(rx.try_recv().is_err());

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"11;#123456\x07".to_vec(),
        }));

        assert!(rx.try_recv().is_err());
        assert_eq!(
            server.clients[&1].host_terminal_theme.background,
            Some(crate::terminal_theme::RgbColor {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            })
        );
        assert_eq!(
            server.app.state.host_terminal_theme.background,
            Some(crate::terminal_theme::RgbColor {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            })
        );
    }

    #[test]
    fn render_and_stream_uses_each_client_terminal_size() {
        let mut server = test_headless_server();
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (desktop_tx, _desktop_control_rx, desktop_rx) = test_client_writer();
        let (phone_tx, _phone_control_rx, phone_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(desktop_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(phone_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        server.render_and_stream();

        let desktop_frame = read_server_frame(desktop_rx.recv().expect("desktop frame"));
        let phone_frame = read_server_frame(phone_rx.recv().expect("phone frame"));

        assert_eq!((desktop_frame.width, desktop_frame.height), (120, 40));
        assert_eq!((phone_frame.width, phone_frame.height), (80, 24));
    }

    #[tokio::test]
    async fn resize_shared_runtime_resizes_background_tabs() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let background_tab = workspace.test_add_tab(Some("background"));
        let active_pane = workspace.tabs[0].root_pane;
        let background_pane = workspace.tabs[background_tab].root_pane;
        workspace.tabs[0].runtimes.insert(
            active_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
        workspace.tabs[background_tab].runtimes.insert(
            background_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        let terminal_area = server.app.state.view.terminal_area;
        let expected = (terminal_area.height, terminal_area.width.saturating_sub(1));
        assert_eq!(
            server
                .app
                .state
                .runtime_for_pane(&server.app.terminal_runtimes, active_pane)
                .unwrap()
                .current_size(),
            expected
        );
        assert_eq!(
            server
                .app
                .state
                .runtime_for_pane(&server.app.terminal_runtimes, background_pane)
                .unwrap()
                .current_size(),
            expected
        );
    }

    #[test]
    fn terminal_attach_disconnect_restores_app_pane_size() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        let terminal_id_string = terminal_id.to_string();
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let expected_app_size = server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size();
        assert_ne!(expected_app_size, (24, 80));

        let (writer, _control_rx, _render_rx) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            writer,
        }));
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 2,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );
        assert_eq!(server.foreground_client_id, Some(1));
        assert!(server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("runtime")
                .current_size(),
            (24, 80)
        );

        assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 2 }));

        assert!(!server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("runtime")
                .current_size(),
            expected_app_size
        );
        drop(server);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    #[test]
    fn render_and_stream_sends_terminal_frame_for_terminal_ansi_client() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();

        match read_server_message(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("terminal frame"),
        ) {
            ServerMessage::Terminal(frame) => {
                assert_eq!(frame.seq, 1);
                assert_eq!((frame.width, frame.height), (80, 24));
                assert!(frame.full);
                assert!(!frame.bytes.is_empty());
            }
            other => panic!("expected terminal frame, got {other:?}"),
        }
        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );
    }

    #[test]
    fn terminal_ansi_input_does_not_reset_blit_baseline() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial terminal frame");
        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );

        assert!(!server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: Vec::new(),
        }));
        server.render_and_stream();

        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn outer_focus_gained_forces_terminal_ansi_full_redraw() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial terminal frame");

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));
        server.render_and_stream();

        match read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()) {
            ServerMessage::Terminal(frame) => {
                assert_eq!(frame.seq, 2);
                assert!(frame.full);
            }
            other => panic!("expected terminal frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn outer_focus_gained_client_render_pending_survives_semantic_render_queue_full() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial semantic frame");

        let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
            .expect("serialize dummy message");
        server
            .clients
            .get(&1)
            .unwrap()
            .writer
            .as_ref()
            .unwrap()
            .render
            .try_send(queued)
            .expect("pre-fill render queue");

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));
        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::Full
        );

        server.render_and_stream();

        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::Full
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::ReloadSoundConfig
        ));

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

        assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 1 }));
        server.render_and_stream();

        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::None
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::Frame(_)
        ));
    }

    #[test]
    fn outer_focus_gained_does_not_force_terminal_ansi_full_redraw_when_disabled() {
        let mut server = test_headless_server();
        server.app.state.redraw_on_focus_gained = false;
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial terminal frame");

        server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        });
        server.render_and_stream();

        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );
    }

    #[test]
    fn outer_focus_gained_does_not_mark_semantic_render_pending_when_disabled() {
        let mut server = test_headless_server();
        server.app.state.redraw_on_focus_gained = false;
        let (client_tx, _client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));

        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::None
        );
        assert!(!server.app.full_redraw_pending);
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
    }

    #[test]
    fn full_render_queue_does_not_advance_terminal_ansi_baseline() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();
        let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
            .expect("serialize dummy message");
        client_tx
            .render
            .try_send(queued)
            .expect("pre-fill render queue");

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();

        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            0
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::ReloadSoundConfig
        ));
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn writer_drained_retries_pending_terminal_ansi_render() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();
        let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
            .expect("serialize dummy message");
        client_tx
            .render
            .try_send(queued)
            .expect("pre-fill render queue");

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();
        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::Full
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::ReloadSoundConfig
        ));

        assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 1 }));
        server.render_and_stream();

        match read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()) {
            ServerMessage::Terminal(frame) => assert_eq!(frame.seq, 1),
            other => panic!("expected terminal frame, got {other:?}"),
        }
        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );
        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::None
        );
    }

    #[test]
    fn render_and_stream_skips_identical_frame_sends() {
        let mut server = test_headless_server();
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        server.render_and_stream();
        let first = client_rx.recv_timeout(Duration::from_millis(100));
        assert!(first.is_ok(), "expected first frame to be sent");

        server.render_and_stream();
        assert!(
            client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "identical frame should not be sent twice"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_streams_dirty_row_from_last_frame() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.render_and_stream();
        let first = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial frame"),
        );
        assert!(first.cells.iter().any(|cell| cell.symbol == "a"));

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(server.render_retained_pty_update_and_stream());
        let patched = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame"),
        );
        assert!(patched.cells.iter().any(|cell| cell.symbol == "Z"));
        assert_eq!((patched.width, patched.height), (80, 24));
    }

    #[tokio::test]
    async fn retained_pty_update_declines_while_popup_is_visible() {
        let (mut server, client_rx, _) = retained_test_server(b"tiled");
        let popup_runtime =
            crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"popup-aaaa");
        let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);

        server.render_and_stream();
        let initial = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial popup frame"),
        );
        assert!(frame_text(&initial).contains("popup-aaaa"));
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes(b"\rZ");

        assert!(!server.render_retained_pty_update_and_stream());
        server.render_and_stream();
        let updated = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full popup fallback frame"),
        );
        assert!(frame_text(&updated).contains("Zopup-aaaa"));
    }

    #[tokio::test]
    async fn popup_forces_host_mouse_capture_for_headless_client() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.app.state.mouse_capture = false;
        let popup_runtime =
            crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"popup");
        server.app.install_test_popup_runtime(popup_runtime);

        server.stream_host_mouse_capture_mode();

        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("mouse capture message")
            ),
            ServerMessage::MouseCapture { enabled: true }
        ));
    }

    #[tokio::test]
    async fn virtual_render_uses_popup_cursor() {
        let (mut server, _client_rx, _) = retained_test_server(b"\x1b[2;2H");
        let popup_runtime =
            crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"\x1b[4;5H");
        let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);

        let (_, cursor) = crate::server::render_stream::render_virtual_with_runtime_registry(
            &mut server.app.state,
            &server.app.terminal_runtimes,
            ratatui::layout::Rect::new(0, 0, 80, 24),
            true,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let (_, inner) =
            crate::ui::popup_pane_rects(&server.app.state, server.app.state.view.terminal_area)
                .unwrap();
        let expected = server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .cursor_state(inner, true)
            .unwrap();

        assert_eq!(
            cursor,
            Some(crate::protocol::CursorState {
                x: expected.x,
                y: expected.y,
                visible: expected.visible,
                shape: expected.shape,
            })
        );
    }

    #[tokio::test]
    async fn virtual_render_does_not_resize_directly_attached_popup() {
        let (mut server, _client_rx, _) = retained_test_server(b"tiled");
        let popup_runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(50, 13, b"");
        let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);
        server
            .app
            .state
            .direct_attach_resize_locks
            .insert(terminal_id.clone());

        let _ = crate::server::render_stream::render_virtual_with_runtime_registry(
            &mut server.app.state,
            &server.app.terminal_runtimes,
            ratatui::layout::Rect::new(0, 0, 80, 24),
            true,
            crate::kitty_graphics::HostCellSize::default(),
        );

        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .current_size(),
            (13, 50)
        );
    }

    #[tokio::test]
    async fn retained_pty_update_declines_while_toast_is_visible() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::NeedsAttention,
            title: "pi needs attention".to_owned(),
            context: "background · 2".to_owned(),
            position: None,
            target: None,
        });
        server.render_and_stream();
        let initial = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial frame"),
        );
        assert!(
            frame_text(&initial).contains("pi needs attention"),
            "expected initial full frame to include toast text"
        );

        let toast_row = server.app.state.view.toast_hit_area.y;
        let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
        let pane_row = toast_row
            .checked_sub(inner_rect.y)
            .expect("toast should overlap the pane")
            + 1;
        assert!(pane_row <= inner_rect.height);
        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(format!("\x1b[{pane_row};1Hzzzz").as_bytes());

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(
            client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "retained path should not stream a frame that can overwrite toast cells"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_declines_while_copy_feedback_is_visible() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.state.copy_feedback = Some(crate::app::state::CopyFeedback {
            message: "copied to clipboard".to_owned(),
        });
        server.render_and_stream();
        let initial = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial frame"),
        );
        let initial_text = frame_text(&initial);
        assert!(
            initial_text.contains("copied to clipboard"),
            "expected initial full frame to include copy feedback"
        );

        let feedback_row = initial_text
            .lines()
            .position(|line| line.contains("copied to clipboard"))
            .expect("copy feedback row") as u16;
        let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
        let pane_row = feedback_row
            .checked_sub(inner_rect.y)
            .expect("copy feedback should overlap the pane")
            + 1;
        assert!(pane_row <= inner_rect.height);
        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(format!("\x1b[{pane_row};1Hzzzz").as_bytes());

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(
            client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "retained path should not stream a frame that can overwrite copy feedback cells"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_matches_full_render_frame() {
        let initial = b"\x1b[6 qleft \xe4\xb8\xad";
        let update = b"\r\x1b[44mZ\x1b[0m";
        let (mut retained_server, retained_rx, retained_pane_id) = retained_test_server(initial);
        let (mut full_server, full_rx, full_pane_id) = retained_test_server(initial);

        retained_server.render_and_stream();
        let _ = retained_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial retained baseline");
        full_server.render_and_stream();
        let _ = full_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial full baseline");

        retained_server
            .app
            .state
            .runtime_for_pane_in_workspace(
                &retained_server.app.terminal_runtimes,
                0,
                retained_pane_id,
            )
            .expect("retained runtime")
            .test_process_pty_bytes(update);
        full_server
            .app
            .state
            .runtime_for_pane_in_workspace(&full_server.app.terminal_runtimes, 0, full_pane_id)
            .expect("full runtime")
            .test_process_pty_bytes(update);

        assert!(retained_server.render_retained_pty_update_and_stream());
        full_server.render_and_stream();

        let retained_frame = read_server_frame(
            retained_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame"),
        );
        let full_frame = read_server_frame(
            full_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full frame"),
        );
        assert_frame_data_eq(&retained_frame, &full_frame);
    }

    #[tokio::test]
    async fn retained_pty_update_streams_cursor_only_change() {
        let initial = b"abcd";
        let update = b"\x1b[D";
        let (mut retained_server, retained_rx, retained_pane_id) = retained_test_server(initial);
        let (mut full_server, full_rx, full_pane_id) = retained_test_server(initial);

        retained_server.render_and_stream();
        let _ = retained_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial retained baseline");
        full_server.render_and_stream();
        let _ = full_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial full baseline");

        retained_server
            .app
            .state
            .runtime_for_pane_in_workspace(
                &retained_server.app.terminal_runtimes,
                0,
                retained_pane_id,
            )
            .expect("retained runtime")
            .test_process_pty_bytes(update);
        full_server
            .app
            .state
            .runtime_for_pane_in_workspace(&full_server.app.terminal_runtimes, 0, full_pane_id)
            .expect("full runtime")
            .test_process_pty_bytes(update);

        assert!(retained_server.render_retained_pty_update_and_stream());
        full_server.render_and_stream();

        let retained_frame = read_server_frame(
            retained_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained cursor frame"),
        );
        let full_frame = read_server_frame(
            full_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full cursor frame"),
        );
        assert_frame_data_eq(&retained_frame, &full_frame);
    }

    #[tokio::test]
    async fn retained_pty_update_declines_unsafe_mode_without_consuming_dirty_rows() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        server.app.state.mode = crate::app::Mode::Navigate;
        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

        server.app.state.mode = crate::app::Mode::Terminal;
        assert!(server.render_retained_pty_update_and_stream());
        let patched = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame after safe mode"),
        );
        assert!(patched.cells.iter().any(|cell| cell.symbol == "Z"));
    }

    #[tokio::test]
    async fn headless_full_render_clears_full_redraw_pending_for_future_retained_updates() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.full_redraw_pending = true;

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("full redraw frame");
        assert!(!server.app.full_redraw_pending);

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(server.render_retained_pty_update_and_stream());
    }

    #[tokio::test]
    async fn retained_pty_update_declines_when_patch_would_stale_hyperlinks() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"link");
        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");
        let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
        let client = server.clients.get_mut(&1).unwrap();
        let mut frame = client.render_state.last_frame().unwrap().clone();
        frame.hyperlinks = vec!["https://example.com".to_owned()];
        let hyperlink_idx =
            usize::from(inner_rect.y) * usize::from(frame.width) + usize::from(inner_rect.x);
        frame.cells[hyperlink_idx].hyperlink = Some(0);
        let prepared = client
            .render_state
            .prepare_frame(frame)
            .expect("hyperlink frame differs");
        client.render_state.commit_sent_frame(prepared);

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rplain");

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

        server.render_and_stream();
        let full = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full frame after hyperlink overwrite"),
        );
        assert!(
            full.cells.iter().all(|cell| cell.hyperlink.is_none()),
            "full render should clear overwritten hyperlink cells"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_allows_dirty_row_that_creates_plain_url() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"plain");
        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rhttps://example.com/new");

        assert!(server.render_retained_pty_update_and_stream());
        let patched = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame after plain URL"),
        );
        assert!(
            patched.hyperlinks.is_empty(),
            "retained render should not synthesize plain URL hyperlink metadata"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_allows_kitty_enabled_empty_graphics_cache() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.state.kitty_graphics_enabled = true;
        server.clients.get_mut(&1).unwrap().cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(server.render_retained_pty_update_and_stream());
        let retained = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame with kitty enabled"),
        );
        assert!(retained.cells.iter().any(|cell| cell.symbol == "Z"));
    }

    #[tokio::test]
    async fn retained_pty_update_declines_when_graphics_cache_has_content() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.state.kitty_graphics_enabled = true;
        let client = server.clients.get_mut(&1).unwrap();
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");
        server
            .clients
            .get_mut(&1)
            .unwrap()
            .graphics_cache
            .test_mark_non_empty();

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[tokio::test]
    async fn full_redraw_pending_survives_full_render_queue_full() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
            .expect("serialize dummy message");
        server
            .clients
            .get(&1)
            .unwrap()
            .writer
            .as_ref()
            .unwrap()
            .render
            .try_send(queued)
            .expect("pre-fill render queue");
        server.app.full_redraw_pending = true;

        server.render_and_stream();

        assert!(server.app.full_redraw_pending);
        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::Full
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::ReloadSoundConfig
        ));

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn client_config_reload_request_refreshes_attached_clients() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.app.state.request_client_config_reload = true;

        server.drain_client_config_reload_request();

        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("client config reload message"),
        ) {
            ServerMessage::ReloadSoundConfig => {}
            other => panic!("expected ReloadSoundConfig, got {other:?}"),
        }
        assert!(!server.app.state.request_client_config_reload);
    }

    #[test]
    fn clipboard_write_targets_foreground_client_only() {
        let mut server = test_headless_server();
        let (background_tx, background_control_rx, _background_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(background_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
            content: b"test".to_vec(),
        });

        assert!(changed);
        assert_eq!(
            server
                .app
                .state
                .copy_feedback
                .as_ref()
                .map(|feedback| feedback.message.as_str()),
            Some("copied to clipboard")
        );
        match read_server_message(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("foreground clipboard message"),
        ) {
            ServerMessage::Clipboard { data } => assert_eq!(data, "dGVzdA=="),
            other => panic!("expected clipboard message, got {other:?}"),
        }
        assert!(
            background_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "background client should not receive clipboard writes"
        );
    }

    #[test]
    fn clipboard_write_without_foreground_client_does_not_show_feedback() {
        let mut server = test_headless_server();
        server.foreground_client_id = None;

        let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
            content: b"test".to_vec(),
        });

        assert!(changed);
        assert!(
            server.app.state.copy_feedback.is_none(),
            "clipboard feedback should only show when a foreground client can receive the write"
        );
    }

    #[test]
    fn clipboard_write_failed_foreground_send_does_not_show_feedback() {
        let mut server = test_headless_server();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();
        drop(foreground_control_rx);

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
            content: b"test".to_vec(),
        });

        assert!(changed);
        assert!(
            server.app.state.copy_feedback.is_none(),
            "clipboard feedback should only show after the foreground client receives the write"
        );
        assert!(
            !server.clients.contains_key(&1),
            "failed targeted send should remove the broken foreground client"
        );
    }

    #[test]
    fn prefix_input_source_targets_foreground_client_only() {
        let mut server = test_headless_server();
        let (background_tx, background_control_rx, _background_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(background_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        // Drain any setup messages (e.g. mouse-capture sync) before exercising the event.
        while foreground_control_rx
            .recv_timeout(Duration::from_millis(20))
            .is_ok()
        {}

        let changed = server
            .handle_internal_event_with_forwarding(AppEvent::PrefixInputSource { active: true });

        assert!(changed);
        match read_server_message(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("foreground prefix input-source message"),
        ) {
            ServerMessage::PrefixInputSource { active } => assert!(active),
            other => panic!("expected prefix input-source message, got {other:?}"),
        }
        assert!(
            background_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "background client should not receive prefix input-source changes"
        );
    }

    #[test]
    fn headless_app_keeps_prefix_input_source_switch_off_process() {
        // An App-internal drain (e.g. the exhaustive drain at the top of
        // handle_api_request) can consume a queued PrefixInputSource intent
        // before the forwarding drain sees it. The headless App must treat the
        // event as inert instead of switching the host input source from the
        // server process.
        struct CountingPrefixInputSource(std::rc::Rc<std::cell::Cell<usize>>);
        impl crate::platform::PrefixInputSource for CountingPrefixInputSource {
            fn switch_to_ascii(&mut self) {
                self.0.set(self.0.get() + 1);
            }
            fn restore(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let mut server = test_headless_server();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        server
            .app
            .set_prefix_input_source(Box::new(CountingPrefixInputSource(calls.clone())));

        server
            .app
            .handle_internal_event(AppEvent::PrefixInputSource { active: true });
        server
            .app
            .handle_internal_event(AppEvent::PrefixInputSource { active: false });
        assert_eq!(
            calls.get(),
            0,
            "headless server must not apply the host input-source switch"
        );

        // Sanity: the same event does apply once the flag is on (monolithic semantics).
        server.app.local_input_source_switch = true;
        server
            .app
            .handle_internal_event(AppEvent::PrefixInputSource { active: true });
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn client_local_notifications_target_foreground_client_only() {
        let mut server = test_headless_server();
        let (background_tx, background_control_rx, _background_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(background_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        assert!(server.send_to_foreground_client(ServerMessage::Notify {
            kind: protocol::NotifyKind::Toast,
            message: "pi finished".to_string(),
            body: Some("workspace 1".to_string()),
        }));

        match read_server_message(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("foreground toast message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
            } => {
                assert_eq!(kind, protocol::NotifyKind::Toast);
                assert_eq!(message, "pi finished");
                assert_eq!(body.as_deref(), Some("workspace 1"));
            }
            other => panic!("expected toast notify, got {other:?}"),
        }
        assert!(
            background_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "background client should not receive client-local notifications"
        );
    }

    #[test]
    fn nagi_toast_delivery_keeps_toast_in_frame_without_client_notify() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Nagi;

        let changed = server.handle_internal_event_with_forwarding(AppEvent::UpdateReady {
            version: "9.9.9".to_string(),
            install_command: "nagi update".into(),
        });

        assert!(changed);
        assert!(server.app.state.toast.is_some());
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "nagi delivery should render in-frame instead of forwarding a client-local notification"
        );
    }

    #[test]
    fn system_toast_delivery_forwards_system_notify_kind() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

        let changed = server.handle_internal_event_with_forwarding(AppEvent::UpdateReady {
            version: "9.9.9".to_string(),
            install_command: "nagi update".into(),
        });

        assert!(changed);
        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("system toast message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "v9.9.9 available");
                assert_eq!(
                    body.as_deref(),
                    Some("detach, run `nagi update`, then follow its restart guidance")
                );
            }
            other => panic!("expected system toast notify, got {other:?}"),
        }
    }

    #[test]
    fn notification_show_api_forwards_system_notification_to_foreground_client() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build failed".into(),
                        body: Some("api workspace".into()),
                        position: Some(crate::config::ToastNagiPosition::TopLeft),
                        sound: api::schema::NotificationShowSound::Request,
                    },
                ),
            },
            respond_to,
            response_write_complete: None,
        });

        assert!(changed);
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: true,
                reason: api::schema::NotificationShowReason::Shown,
            }
        );
        let first = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("api notification message"),
        );
        let second = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("api sound message"),
        );

        match first {
            ServerMessage::Notify {
                kind,
                message,
                body,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "build failed");
                assert_eq!(body.as_deref(), Some("api workspace"));
            }
            other => panic!("expected api notification, got {other:?}"),
        }
        match second {
            ServerMessage::Notify {
                kind,
                message,
                body,
            } => {
                assert_eq!(kind, protocol::NotifyKind::Sound);
                assert_eq!(message, "agent attention");
                assert!(body.is_none());
            }
            other => panic!("expected api sound, got {other:?}"),
        }
    }

    #[test]
    fn notification_show_api_preserves_colon_in_forwarded_title() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build: failed".into(),
                        body: Some("api workspace".into()),
                        position: None,
                        sound: api::schema::NotificationShowSound::None,
                    },
                ),
            },
            respond_to,
            response_write_complete: None,
        });

        assert!(changed);
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: true,
                reason: api::schema::NotificationShowReason::Shown,
            }
        );
        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("api notification message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "build: failed");
                assert_eq!(body.as_deref(), Some("api workspace"));
            }
            other => panic!("expected api notification, got {other:?}"),
        }
    }

    #[test]
    fn notification_show_api_validates_empty_title_before_disabled_delivery() {
        let mut server = test_headless_server();
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Off;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "\n\t".into(),
                        body: None,
                        position: None,
                        sound: api::schema::NotificationShowSound::None,
                    },
                ),
            },
            respond_to,
            response_write_complete: None,
        });

        assert!(changed);
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed.error.code, "invalid_params");
        assert_eq!(parsed.error.message, "notification title is empty");
    }

    #[test]
    fn notification_show_api_reports_no_foreground_client() {
        let mut server = test_headless_server();
        server.foreground_client_id = None;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build failed".into(),
                        body: None,
                        position: None,
                        sound: api::schema::NotificationShowSound::Request,
                    },
                ),
            },
            respond_to,
            response_write_complete: None,
        });

        assert!(changed);
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: false,
                reason: api::schema::NotificationShowReason::NoForegroundClient,
            }
        );
    }

    #[test]
    fn notification_show_api_nagi_toast_expires_headless() {
        let mut server = test_headless_server();
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Nagi;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "notify".into(),
                    method: api::schema::Method::NotificationShow(
                        api::schema::NotificationShowParams {
                            title: "build failed".into(),
                            body: None,
                            position: None,
                            sound: api::schema::NotificationShowSound::None,
                        },
                    ),
                },
                respond_to,
                response_write_complete: None,
            })
        );

        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: true,
                reason: api::schema::NotificationShowReason::Shown,
            }
        );
        let deadline = server.app.toast_deadline.expect("api toast deadline");
        assert!(server.handle_scheduled_tasks_headless(deadline, false));
        assert!(server.app.state.toast.is_none());
        assert!(server.app.toast_deadline.is_none());
    }

    #[test]
    fn notification_show_api_forwards_sound_for_nagi_delivery() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Nagi;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "notify".into(),
                    method: api::schema::Method::NotificationShow(
                        api::schema::NotificationShowParams {
                            title: "build failed".into(),
                            body: None,
                            position: None,
                            sound: api::schema::NotificationShowSound::Done,
                        },
                    ),
                },
                respond_to,
                response_write_complete: None,
            })
        );

        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: true,
                reason: api::schema::NotificationShowReason::Shown,
            }
        );
        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("api sound message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
            } => {
                assert_eq!(kind, protocol::NotifyKind::Sound);
                assert_eq!(message, "agent done");
                assert!(body.is_none());
            }
            other => panic!("expected api sound, got {other:?}"),
        }
    }

    #[test]
    fn delayed_agent_notification_forwards_after_deadline() {
        let mut server = test_headless_server();
        let background = crate::workspace::Workspace::test_new("background");
        let pane_id = background.tabs[0].root_pane;
        let foreground = crate::workspace::Workspace::test_new("foreground");
        server.app.state.workspaces = vec![background, foreground];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;
        server.app.state.toast_config.delay_seconds = 1;

        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        let changed = server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Pi),
            state: crate::detect::AgentState::Blocked,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: Instant::now(),
        });

        assert!(changed);
        assert!(server.app.state.toast.is_none());
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "delayed transition should not notify immediately"
        );

        let deadline = server
            .app
            .state
            .next_pending_agent_notification_deadline()
            .expect("pending notification deadline");
        assert!(server.handle_scheduled_tasks_headless(deadline, false));

        let first = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("delayed sound message"),
        );
        let second = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("delayed toast message"),
        );

        assert!(matches!(
            first,
            ServerMessage::Notify {
                kind: protocol::NotifyKind::Sound,
                ..
            }
        ));
        match second {
            ServerMessage::Notify {
                kind,
                message,
                body,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "pi needs attention");
                assert_eq!(body.as_deref(), Some("background · 1"));
            }
            other => panic!("expected delayed system toast, got {other:?}"),
        }
        assert!(server.app.state.pending_agent_notifications.is_empty());
    }

    #[test]
    fn delayed_active_tab_unfocused_agent_notification_forwards_after_deadline() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("active");
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;
        server.app.state.toast_config.delay_seconds = 1;

        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(false),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
                pane_id,
                agent: Some(crate::detect::Agent::Pi),
                state: crate::detect::AgentState::Blocked,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: Instant::now(),
            })
        );
        assert!(server.app.state.toast.is_none());
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "delayed transition should not notify immediately"
        );

        let deadline = server
            .app
            .state
            .next_pending_agent_notification_deadline()
            .expect("pending notification deadline");
        assert!(server.handle_scheduled_tasks_headless(deadline, false));

        let first = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("delayed sound message"),
        );
        let second = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("delayed toast message"),
        );

        assert!(matches!(
            first,
            ServerMessage::Notify {
                kind: protocol::NotifyKind::Sound,
                ..
            }
        ));
        match second {
            ServerMessage::Notify {
                kind,
                message,
                body,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "pi needs attention");
                assert_eq!(body.as_deref(), Some("active · 1"));
            }
            other => panic!("expected delayed system toast, got {other:?}"),
        }
    }

    #[test]
    fn stale_api_agent_report_does_not_forward_done_sound() {
        let mut server = test_headless_server();
        let background = crate::workspace::Workspace::test_new("background");
        let pane_id = background.tabs[0].root_pane;
        let public_pane_id = format!("{}:p1", background.id);
        let foreground = crate::workspace::Workspace::test_new("foreground");
        server.app.state.workspaces = vec![background, foreground];
        server.app.state.ensure_test_terminals();
        let terminal_id = server.app.state.workspaces[0]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_hook_authority(
                "nagi:pi".into(),
                "pi".into(),
                crate::detect::AgentState::Working,
                None,
                Some(20),
            );
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "stale".into(),
                method: api::schema::Method::PaneReportAgent(api::schema::PaneReportAgentParams {
                    pane_id: public_pane_id,
                    source: "nagi:pi".into(),
                    agent: "pi".into(),
                    state: api::schema::PaneAgentState::Idle,
                    message: None,
                    seq: Some(19),
                    agent_session_id: None,
                    agent_session_path: None,
                }),
            },
            respond_to,
            response_write_complete: None,
        });

        assert!(changed);
        assert!(response_rx.recv_timeout(Duration::from_millis(100)).is_ok());
        assert_eq!(
            server.app.state.terminals.get(&terminal_id).unwrap().state,
            crate::detect::AgentState::Working
        );
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "stale idle report must not forward a done sound"
        );
    }

    #[test]
    fn mission_create_and_configure_report_render_changes_only_once() {
        use crate::api::schema::{
            Method, MissionCheck, MissionConfigureParams, MissionCreateParams, MissionPathRule,
        };

        let mut server = test_headless_server();
        let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let create = Method::MissionCreate(MissionCreateParams {
            mission_id: "mission-render-change".into(),
            title: "Refresh the cockpit".into(),
            repository_path: repository.to_string_lossy().into_owned(),
            objective: "Render durable mission changes immediately".into(),
            acceptance_criteria: vec!["The cockpit refreshes exactly once".into()],
        });
        assert!(
            crate::server::mission_bridge::handle(
                &mut server.mission_runtime,
                "mission-create-first",
                &create,
            )
            .unwrap()
            .changed
        );
        assert!(
            !crate::server::mission_bridge::handle(
                &mut server.mission_runtime,
                "mission-create-duplicate",
                &create,
            )
            .unwrap()
            .changed
        );

        let configure = Method::MissionConfigure(MissionConfigureParams {
            mission_id: "mission-render-change".into(),
            checks: vec![MissionCheck::Command {
                id: "render-test".into(),
                program: "true".into(),
                args: Vec::new(),
                cwd: ".".into(),
                relevant_paths: vec![MissionPathRule::All],
                required_artifacts: Vec::new(),
                include_ignored: false,
                required: true,
                covers: vec![0],
            }],
        });
        assert!(
            crate::server::mission_bridge::handle(
                &mut server.mission_runtime,
                "mission-configure-first",
                &configure,
            )
            .unwrap()
            .changed
        );
        assert!(
            !crate::server::mission_bridge::handle(
                &mut server.mission_runtime,
                "mission-configure-duplicate",
                &configure,
            )
            .unwrap()
            .changed
        );
    }

    #[test]
    fn disabled_mission_runtime_rejects_start_and_respond_as_feature_unavailable() {
        use crate::api::schema::{
            MissionProvider, MissionProviderMode, MissionRespondParams, MissionResponseDecision,
            MissionStartParams,
        };

        let mut server = test_headless_server_with_mission_runtime(
            crate::mission::runtime::MissionRuntime::disabled(),
        );
        let start = server.handle_mission_start_api(
            "mission-start-disabled",
            &MissionStartParams {
                mission_id: "mission-1".to_owned(),
                run_id: "run-1".to_owned(),
                provider: MissionProvider::Codex,
                mode: MissionProviderMode::Managed,
                worktree_path: None,
                execute_declared_checks: false,
                execute_project_recipe: false,
            },
        );
        let respond = server.handle_mission_respond_api(
            "mission-respond-disabled",
            &MissionRespondParams {
                mission_id: "mission-1".to_owned(),
                run_id: "run-1".to_owned(),
                attention_id: "attention-1".to_owned(),
                decision: MissionResponseDecision::Deny,
                answers: Default::default(),
            },
        );

        for response in [start, respond] {
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["error"]["code"], "feature_unavailable");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_mission_start_wires_the_registered_opencode_adapter() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::{
            api::schema::{MissionProvider, MissionProviderMode, MissionStartParams},
            managed_provider::ProviderEvent,
            mission::{model::MissionStatus, runtime::CreateMission},
        };

        fn git(repository: &std::path::Path, arguments: &[&str]) {
            let status = std::process::Command::new("git")
                .args(arguments)
                .current_dir(repository)
                .status()
                .expect("run git fixture command");
            assert!(
                status.success(),
                "git fixture command failed: {arguments:?}"
            );
        }

        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "-q"]);
        git(repository.path(), &["config", "user.name", "Test User"]);
        git(
            repository.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repository.path().join("README.md"), "fixture\n").unwrap();
        git(repository.path(), &["add", "README.md"]);
        git(repository.path(), &["commit", "-qm", "fixture"]);

        let provider_directory = tempfile::tempdir().unwrap();
        let executable = provider_directory.path().join("opencode-conformance");
        std::fs::write(
            &executable,
            include_str!("../../tests/fixtures/providers/opencode.py"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let mut server = test_headless_server();
        server
            .mission_runtime
            .create_mission(CreateMission {
                mission_id: "mission-opencode".into(),
                title: "Start OpenCode".into(),
                repository_path: repository.path().to_string_lossy().into_owned(),
                objective: "Run through the registered adapter".into(),
                acceptance_criteria: vec!["The provider session is bound".into()],
                at_millis: 1,
            })
            .unwrap();
        configure_test_mission(&mut server.mission_runtime, "mission-opencode", 1);
        server.managed_provider_executable = Some(executable);

        let denied = server.handle_mission_start_api(
            "start-opencode-project-recipe-without-local-consent",
            &MissionStartParams {
                mission_id: "mission-opencode".into(),
                run_id: "run-opencode".into(),
                provider: MissionProvider::OpenCode,
                mode: MissionProviderMode::Managed,
                worktree_path: None,
                execute_declared_checks: false,
                execute_project_recipe: true,
            },
        );
        let denied: crate::api::schema::ErrorResponse = serde_json::from_str(&denied).unwrap();
        assert_eq!(denied.error.code, "interactive_consent_required");

        let response = server.handle_mission_start_api(
            "start-opencode",
            &MissionStartParams {
                mission_id: "mission-opencode".into(),
                run_id: "run-opencode".into(),
                provider: MissionProvider::OpenCode,
                mode: MissionProviderMode::Managed,
                worktree_path: None,
                execute_declared_checks: false,
                execute_project_recipe: false,
            },
        );
        let response: crate::api::schema::SuccessResponse = serde_json::from_str(&response)
            .expect("OpenCode must return the typed mission start response");
        assert!(matches!(
            response.result,
            crate::api::schema::ResponseResult::MissionRunStarted { .. }
        ));
        assert!(server.managed_runs.contains_key("run-opencode"));

        let event = tokio::time::timeout(PROVIDER_FIXTURE_TIMEOUT, server.provider_event_rx.recv())
            .await
            .expect("OpenCode ready timeout")
            .expect("OpenCode event channel closed");
        assert!(matches!(
            event,
            ProviderEvent::Ready {
                ref run_id,
                ref session_id,
            } if run_id == "run-opencode" && session_id == "session-live"
        ));
        assert!(server.handle_provider_event(event));
        let mission = server.mission_runtime.mission("mission-opencode").unwrap();
        assert_eq!(mission.status, MissionStatus::Active);
        assert_eq!(
            mission
                .run
                .as_ref()
                .and_then(|run| run.provider_session_id.as_deref()),
            Some("session-live")
        );
        server.release_managed_run("run-opencode");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_handoff_starts_the_target_provider_from_the_inspected_artifact() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::{
            api::schema::{MissionHandoffStartParams, MissionProvider},
            managed_provider::ProviderEvent,
            mission::{
                claims::ClaimRequestId,
                handoff::build_preview,
                model::{MissionStatus, ProviderKind, ProviderMode},
                runtime::{CreateMission, StartRun},
            },
        };

        fn git(repository: &std::path::Path, arguments: &[&str]) {
            assert!(std::process::Command::new("git")
                .args(arguments)
                .current_dir(repository)
                .status()
                .unwrap()
                .success());
        }

        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "-q"]);
        git(repository.path(), &["config", "user.name", "Test User"]);
        git(
            repository.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repository.path().join("README.md"), "fixture\n").unwrap();
        git(repository.path(), &["add", "README.md"]);
        git(repository.path(), &["commit", "-qm", "fixture"]);

        let provider_directory = tempfile::tempdir().unwrap();
        let executable = provider_directory.path().join("opencode-handoff");
        std::fs::write(
            &executable,
            include_str!("../../tests/fixtures/providers/opencode.py"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let mut server = test_headless_server();
        server
            .mission_runtime
            .create_mission(CreateMission {
                mission_id: "mission-handoff-live".into(),
                title: "Continue live".into(),
                repository_path: repository.path().to_string_lossy().into_owned(),
                objective: "Continue the same mission with OpenCode".into(),
                acceptance_criteria: vec!["The target provider receives the handoff".into()],
                at_millis: 1,
            })
            .unwrap();
        configure_test_mission(&mut server.mission_runtime, "mission-handoff-live", 1);
        let source = server
            .mission_runtime
            .start_run(StartRun {
                mission_id: "mission-handoff-live".into(),
                run_id: "run-source".into(),
                provider: ProviderKind::Codex,
                mode: ProviderMode::Managed,
                worktree_path: repository.path().to_string_lossy().into_owned(),
                request_id: ClaimRequestId::new("claim-source").unwrap(),
                execute_declared_checks: false,
                execute_project_recipe: false,
                at_millis: 2,
            })
            .unwrap();
        server
            .mission_runtime
            .transition_run("mission-handoff-live", MissionStatus::Blocked, 3)
            .unwrap();
        server
            .mission_runtime
            .release_worktree(&source.lease)
            .unwrap();
        server.managed_provider_executable = Some(executable);

        let generated_at_millis = 4;
        let mission = server
            .mission_runtime
            .mission("mission-handoff-live")
            .unwrap();
        let artifact = build_preview(
            &mission,
            &server.mission_runtime.attention_items(),
            crate::server::mission_bridge::mission_view(mission.clone()).checks,
            MissionProvider::OpenCode,
            generated_at_millis,
        )
        .unwrap();
        let stale = server.handle_mission_handoff_start_api(
            "handoff-stale",
            &MissionHandoffStartParams {
                mission_id: "mission-handoff-live".into(),
                to: MissionProvider::OpenCode,
                generated_at_millis,
                artifact_sha256: "0".repeat(64),
            },
        );
        let stale: crate::api::schema::ErrorResponse = serde_json::from_str(&stale).unwrap();
        assert_eq!(stale.error.code, "handoff_artifact_changed");
        assert!(server.managed_runs.is_empty());
        assert_eq!(
            server
                .mission_runtime
                .mission("mission-handoff-live")
                .unwrap()
                .run
                .as_ref()
                .unwrap()
                .run_id,
            "run-source"
        );
        let response = server.handle_mission_handoff_start_api(
            "handoff-live",
            &MissionHandoffStartParams {
                mission_id: "mission-handoff-live".into(),
                to: MissionProvider::OpenCode,
                generated_at_millis,
                artifact_sha256: artifact.artifact_sha256.clone(),
            },
        );
        let response: crate::api::schema::SuccessResponse =
            serde_json::from_str(&response).expect("handoff must return a typed success response");
        assert!(matches!(
            response.result,
            crate::api::schema::ResponseResult::MissionRunStarted { .. }
        ));
        assert!(server.managed_runs.contains_key(&artifact.suggested_run_id));

        let event = tokio::time::timeout(PROVIDER_FIXTURE_TIMEOUT, server.provider_event_rx.recv())
            .await
            .expect("OpenCode handoff ready timeout")
            .expect("OpenCode handoff event channel closed");
        assert!(matches!(
            event,
            ProviderEvent::Ready { ref run_id, .. } if run_id == &artifact.suggested_run_id
        ));
        assert!(server.handle_provider_event(event));
        let mission = server
            .mission_runtime
            .mission("mission-handoff-live")
            .unwrap();
        assert_eq!(mission.status, MissionStatus::Active);
        assert_eq!(mission.run_history.len(), 1);
        assert_eq!(
            mission
                .run
                .as_ref()
                .and_then(|run| run.handoff_from_run_id.as_deref()),
            Some("run-source")
        );
        assert_eq!(
            mission
                .run
                .as_ref()
                .and_then(|run| run.handoff_artifact_sha256.as_deref()),
            Some(artifact.artifact_sha256.as_str())
        );
        server.release_managed_run(&artifact.suggested_run_id);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn new_mission_launch_creates_an_isolated_worktree_before_provider_start() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::{
            api::schema::{
                MissionCheck, MissionConfigureParams, MissionCreateParams, MissionPathRule,
                MissionProvider, MissionProviderMode, MissionStartParams,
            },
            app::state::NewMissionLaunchRequest,
            managed_provider::ProviderEvent,
            mission::model::MissionStatus,
        };

        fn git(repository: &std::path::Path, arguments: &[&str]) {
            assert!(std::process::Command::new("git")
                .args(arguments)
                .current_dir(repository)
                .status()
                .unwrap()
                .success());
        }

        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "-q"]);
        git(repository.path(), &["config", "user.name", "Test User"]);
        git(
            repository.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repository.path().join("README.md"), "fixture\n").unwrap();
        std::fs::create_dir(repository.path().join(".nagi")).unwrap();
        std::fs::write(
            repository.path().join(".nagi/project.toml"),
            "schema = 1\n[setup]\ncommand = [\"./project-setup\"]\ntimeout_seconds = 5\n",
        )
        .unwrap();
        let setup = repository.path().join("project-setup");
        std::fs::write(&setup, "#!/bin/sh\nprintf ready > recipe-ready\n").unwrap();
        std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o700)).unwrap();
        git(repository.path(), &["add", "."]);
        git(repository.path(), &["commit", "-qm", "fixture"]);
        let repository = repository.path().canonicalize().unwrap();

        let provider_directory = tempfile::tempdir().unwrap();
        let executable = provider_directory.path().join("opencode-conformance");
        std::fs::write(
            &executable,
            include_str!("../../tests/fixtures/providers/opencode.py"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let worktrees = tempfile::tempdir().unwrap();
        let mut server = test_headless_server();
        server.app.state.worktree_directory = worktrees.path().to_path_buf();
        server.managed_provider_executable = Some(executable);
        server.begin_new_mission_launch(NewMissionLaunchRequest {
            create: MissionCreateParams {
                mission_id: "isolated-launch".into(),
                title: "Isolated launch".into(),
                repository_path: repository.to_string_lossy().into_owned(),
                objective: "Start away from the source checkout".into(),
                acceptance_criteria: vec!["The provider uses a linked worktree".into()],
            },
            configure: MissionConfigureParams {
                mission_id: "isolated-launch".into(),
                checks: vec![MissionCheck::Command {
                    id: "git-proof".into(),
                    program: "git".into(),
                    args: vec!["diff".into(), "--check".into()],
                    cwd: ".".into(),
                    relevant_paths: vec![MissionPathRule::All],
                    required_artifacts: Vec::new(),
                    include_ignored: false,
                    required: true,
                    covers: vec![0],
                }],
            },
            start: MissionStartParams {
                mission_id: "isolated-launch".into(),
                run_id: "run-isolated-launch".into(),
                provider: MissionProvider::OpenCode,
                mode: MissionProviderMode::Managed,
                worktree_path: None,
                execute_declared_checks: true,
                execute_project_recipe: true,
            },
            workspace_write_confirmed: true,
            branch: "mission/isolated-launch".into(),
        });
        assert!(server
            .pending_mission_launches
            .contains_key("isolated-launch"));

        tokio::time::timeout(Duration::from_secs(10), async {
            while server
                .pending_mission_launches
                .contains_key("isolated-launch")
                || server
                    .pending_project_launches
                    .contains_key("isolated-launch")
            {
                if let Ok(event) = server.app.event_rx.try_recv() {
                    server.handle_internal_event_with_forwarding(event);
                }
                server.finish_pending_mission_launches();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worktree provisioning timed out");

        let mission = server.mission_runtime.mission("isolated-launch").unwrap();
        let run = mission.run.as_ref().unwrap();
        assert_ne!(std::path::Path::new(&run.worktree_path), repository);
        assert!(std::path::Path::new(&run.worktree_path)
            .join(".git")
            .exists());
        assert_eq!(
            std::fs::read_to_string(std::path::Path::new(&run.worktree_path).join("recipe-ready"))
                .unwrap(),
            "ready"
        );
        assert_eq!(mission.status, MissionStatus::Preparing);
        assert!(server.managed_runs.contains_key("run-isolated-launch"));

        let event = tokio::time::timeout(PROVIDER_FIXTURE_TIMEOUT, server.provider_event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            ProviderEvent::Ready { ref run_id, .. } if run_id == "run-isolated-launch"
        ));
        assert!(server.handle_provider_event(event));
        assert_eq!(
            server
                .mission_runtime
                .mission("isolated-launch")
                .unwrap()
                .status,
            MissionStatus::Active
        );
    }

    #[test]
    fn managed_response_is_durable_before_send_and_resolves_only_after_provider_ack() {
        use crate::{
            api::schema::{
                MissionProvider, MissionProviderMode, MissionRespondParams,
                MissionResponseDecision, MissionStartParams,
            },
            managed_provider::{
                AttentionClass, ManagedProviderHandle, ProviderCommand, ProviderEvent,
                ProviderResponse, ResponseToken,
            },
            mission::{
                attention::AttentionRisk,
                claims::ClaimRequestId,
                model::{ProviderKind, ProviderMode},
                runtime::{CreateMission, StartRun},
                store::{PersistableMissionEvent, PersistedAttentionState},
            },
        };

        fn git(repo: &std::path::Path, args: &[&str]) {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .expect("run git fixture command");
            assert!(status.success(), "git fixture command failed: {args:?}");
        }

        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "-q"]);
        git(repository.path(), &["config", "user.name", "Test User"]);
        git(
            repository.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repository.path().join("README.md"), "fixture\n").unwrap();
        git(repository.path(), &["add", "README.md"]);
        git(repository.path(), &["commit", "-qm", "fixture"]);

        let mut server = test_headless_server();
        server
            .mission_runtime
            .create_mission(CreateMission {
                mission_id: "mission-respond".into(),
                title: "Respond safely".into(),
                repository_path: repository.path().to_string_lossy().into_owned(),
                objective: "Prove the response lifecycle".into(),
                acceptance_criteria: vec!["Provider acknowledgement is durable".into()],
                at_millis: 1,
            })
            .unwrap();
        configure_test_mission(&mut server.mission_runtime, "mission-respond", 1);
        let started = server
            .mission_runtime
            .start_run(StartRun {
                mission_id: "mission-respond".into(),
                run_id: "run-respond".into(),
                provider: ProviderKind::Codex,
                mode: ProviderMode::Managed,
                worktree_path: repository.path().to_string_lossy().into_owned(),
                request_id: ClaimRequestId::new("request-respond").unwrap(),
                execute_declared_checks: false,
                execute_project_recipe: false,
                at_millis: 2,
            })
            .unwrap();
        server
            .mission_runtime
            .bind_provider_session("mission-respond", "run-respond", "session-respond", 3)
            .unwrap();
        server
            .mission_runtime
            .commit(
                "attention-open-test",
                PersistableMissionEvent::AttentionChanged {
                    mission_id: "mission-respond".into(),
                    attention_id: "attention-respond".into(),
                    state: PersistedAttentionState::Open,
                    risk: AttentionRisk::High,
                    at_millis: 4,
                },
            )
            .unwrap();

        let (handle, mut commands) = ManagedProviderHandle::for_test(4);
        server.managed_runs.insert(
            "run-respond".into(),
            ManagedRun {
                mission_id: "mission-respond".into(),
                provider: ProviderKind::Codex,
                recovered: false,
                execute_declared_checks: false,
                execute_project_recipe: false,
                handle,
                lease: started.lease,
                responses: [(
                    "attention-respond".into(),
                    AvailableManagedResponse {
                        token: ResponseToken::for_test(41, "item/commandExecution/requestApproval"),
                        class: AttentionClass::CommandApproval,
                        session_id: "session-respond".into(),
                        requested_action: "Run tests".into(),
                        questions: Vec::new(),
                        created_at_millis: 1,
                    },
                )]
                .into_iter()
                .collect(),
                inflight_responses: HashMap::new(),
            },
        );
        assert!(server.sync_mission_projection());
        let projected = server
            .app
            .state
            .attention_items
            .iter()
            .find(|item| item.attention_id == "attention-respond")
            .expect("durable attention must reach the cockpit projection");
        assert_eq!(projected.requested_action, "Run tests");
        assert_eq!(
            projected.response_capability,
            crate::api::schema::AttentionResponseCapabilityV1::Reliable
        );
        assert_eq!(projected.risk, crate::api::schema::AttentionRiskV1::High);
        let listed: crate::api::schema::SuccessResponse =
            serde_json::from_str(&server.handle_attention_list_api(
                "attention-list",
                &crate::api::schema::AttentionListParams::default(),
            ))
            .unwrap();
        assert!(matches!(
            listed.result,
            crate::api::schema::ResponseResult::AttentionList { ref items }
                if items.len() == 1 && items[0].attention_id == "attention-respond"
        ));
        let fetched: crate::api::schema::SuccessResponse =
            serde_json::from_str(&server.handle_attention_get_api(
                "attention-get",
                &crate::api::schema::AttentionTarget {
                    attention_id: "attention-respond".into(),
                },
            ))
            .unwrap();
        assert!(matches!(
            fetched.result,
            crate::api::schema::ResponseResult::AttentionInfo { ref item }
                if item.requested_action == "Run tests"
        ));

        let retry = server.handle_mission_start_api(
            "start-retry",
            &MissionStartParams {
                mission_id: "mission-respond".into(),
                run_id: "run-respond".into(),
                provider: MissionProvider::Codex,
                mode: MissionProviderMode::Managed,
                worktree_path: Some(repository.path().to_string_lossy().into_owned()),
                execute_declared_checks: false,
                execute_project_recipe: false,
            },
        );
        let retry: crate::api::schema::SuccessResponse = serde_json::from_str(&retry).unwrap();
        assert!(matches!(
            retry.result,
            crate::api::schema::ResponseResult::MissionRunStarted { .. }
        ));
        assert!(
            commands.try_recv().is_err(),
            "an exact start retry must not send a duplicate provider command"
        );

        let respond_params = MissionRespondParams {
            mission_id: "mission-respond".into(),
            run_id: "run-respond".into(),
            attention_id: "attention-respond".into(),
            decision: MissionResponseDecision::ApproveOnce,
            answers: Default::default(),
        };
        let rejected = server.handle_mission_respond_api("respond-forged", &respond_params);
        let rejected: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(rejected.error.code, "interactive_consent_required");
        assert!(commands.try_recv().is_err());
        assert_eq!(
            server
                .mission_runtime
                .next_response_attempt("mission-respond", "attention-respond", 1)
                .unwrap(),
            1,
            "public API rejection must not persist a response attempt"
        );

        let encoded = server.handle_mission_respond_authorized("respond-request", &respond_params);
        let response: crate::api::schema::SuccessResponse = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            response.result,
            crate::api::schema::ResponseResult::MissionResponseQueued { .. }
        ));
        assert_eq!(
            server
                .mission_runtime
                .mission("mission-respond")
                .unwrap()
                .unresolved_attention_count,
            1,
            "queued provider write must not resolve attention"
        );
        let command = commands.try_recv().expect("provider response command");
        assert!(matches!(
            command,
            ProviderCommand::Respond {
                response: ProviderResponse::Approve,
                ..
            }
        ));

        assert!(
            server.handle_provider_event(ProviderEvent::ResponseResolved {
                run_id: "run-respond".into(),
                request_id: "number:41".into(),
            })
        );
        assert_eq!(
            server
                .mission_runtime
                .mission("mission-respond")
                .unwrap()
                .unresolved_attention_count,
            0,
            "provider acknowledgement must resolve attention durably"
        );

        server
            .mission_runtime
            .commit(
                "attention-open-unknown-test",
                PersistableMissionEvent::AttentionChanged {
                    mission_id: "mission-respond".into(),
                    attention_id: "attention-unknown".into(),
                    state: PersistedAttentionState::Open,
                    risk: AttentionRisk::High,
                    at_millis: crate::server::mission_bridge::now_millis(),
                },
            )
            .unwrap();
        server
            .managed_runs
            .get_mut("run-respond")
            .unwrap()
            .responses
            .insert(
                "attention-unknown".into(),
                AvailableManagedResponse {
                    token: ResponseToken::for_test(42, "item/commandExecution/requestApproval"),
                    class: AttentionClass::CommandApproval,
                    session_id: "session-respond".into(),
                    requested_action: "Run checks".into(),
                    questions: Vec::new(),
                    created_at_millis: 2,
                },
            );
        let encoded = server.handle_mission_respond_authorized(
            "respond-unknown",
            &MissionRespondParams {
                mission_id: "mission-respond".into(),
                run_id: "run-respond".into(),
                attention_id: "attention-unknown".into(),
                decision: MissionResponseDecision::Deny,
                answers: Default::default(),
            },
        );
        let response: crate::api::schema::SuccessResponse = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            response.result,
            crate::api::schema::ResponseResult::MissionResponseQueued { .. }
        ));
        assert!(matches!(
            commands.try_recv().unwrap(),
            ProviderCommand::Respond {
                response: ProviderResponse::Decline,
                ..
            }
        ));
        assert!(
            server.handle_provider_event(ProviderEvent::TransportFailed {
                run_id: "run-respond".into(),
                reason: crate::managed_provider::TransportFailure::Disconnected,
            })
        );
        assert!(
            server
                .mission_runtime
                .next_response_attempt("mission-respond", "attention-unknown", 1)
                .is_err(),
            "unknown delivery must require reconciliation, never automatic retry"
        );
    }

    #[test]
    fn completed_managed_run_releases_its_checkout_for_the_next_mission() {
        use crate::{
            managed_provider::{
                ManagedProviderHandle, ProviderCommand, ProviderEvent, TurnOutcome,
            },
            mission::{
                claims::ClaimRequestId,
                model::{MissionStatus, ProviderKind, ProviderMode},
                runtime::{CreateMission, StartRun},
            },
        };

        fn git(repo: &std::path::Path, args: &[&str]) {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .expect("run git fixture command");
            assert!(status.success(), "git fixture command failed: {args:?}");
        }

        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "-q"]);
        git(repository.path(), &["config", "user.name", "Test User"]);
        git(
            repository.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repository.path().join("README.md"), "fixture\n").unwrap();
        git(repository.path(), &["add", "README.md"]);
        git(repository.path(), &["commit", "-qm", "fixture"]);

        let mut server = test_headless_server();
        server
            .mission_runtime
            .create_mission(CreateMission {
                mission_id: "mission-completed".into(),
                title: "Finish cleanly".into(),
                repository_path: repository.path().to_string_lossy().into_owned(),
                objective: "Release the checkout after review begins".into(),
                acceptance_criteria: vec!["Another mission can use the checkout".into()],
                at_millis: 1,
            })
            .unwrap();
        configure_test_mission(&mut server.mission_runtime, "mission-completed", 1);
        let started = server
            .mission_runtime
            .start_run(StartRun {
                mission_id: "mission-completed".into(),
                run_id: "run-completed".into(),
                provider: ProviderKind::Codex,
                mode: ProviderMode::Managed,
                worktree_path: repository.path().to_string_lossy().into_owned(),
                request_id: ClaimRequestId::new("request-completed").unwrap(),
                execute_declared_checks: false,
                execute_project_recipe: false,
                at_millis: 2,
            })
            .unwrap();
        server
            .mission_runtime
            .bind_provider_session("mission-completed", "run-completed", "session-completed", 3)
            .unwrap();
        let (handle, mut commands) = ManagedProviderHandle::for_test(1);
        server.managed_runs.insert(
            "run-completed".into(),
            ManagedRun {
                mission_id: "mission-completed".into(),
                provider: ProviderKind::Codex,
                recovered: false,
                execute_declared_checks: false,
                execute_project_recipe: false,
                handle,
                lease: started.lease,
                responses: HashMap::new(),
                inflight_responses: HashMap::new(),
            },
        );

        assert!(server.handle_provider_event(ProviderEvent::TurnCompleted {
            run_id: "run-completed".into(),
            turn_id: "turn-completed".into(),
            outcome: TurnOutcome::Completed,
        }));
        assert_eq!(
            server
                .mission_runtime
                .mission("mission-completed")
                .unwrap()
                .status,
            MissionStatus::ReviewRequired
        );
        assert!(!server.managed_runs.contains_key("run-completed"));
        assert_eq!(commands.try_recv().unwrap(), ProviderCommand::Shutdown);

        server
            .mission_runtime
            .create_mission(CreateMission {
                mission_id: "mission-next".into(),
                title: "Use the released checkout".into(),
                repository_path: repository.path().to_string_lossy().into_owned(),
                objective: "Prove the previous lease was released".into(),
                acceptance_criteria: vec!["The next run starts".into()],
                at_millis: 5,
            })
            .unwrap();
        configure_test_mission(&mut server.mission_runtime, "mission-next", 5);
        let next = server
            .mission_runtime
            .start_run(StartRun {
                mission_id: "mission-next".into(),
                run_id: "run-next".into(),
                provider: ProviderKind::ClaudeCode,
                mode: ProviderMode::Managed,
                worktree_path: repository.path().to_string_lossy().into_owned(),
                request_id: ClaimRequestId::new("request-next").unwrap(),
                execute_declared_checks: false,
                execute_project_recipe: false,
                at_millis: 6,
            })
            .expect("completed runs must release their checkout lease");
        server
            .mission_runtime
            .release_worktree(&next.lease)
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn consented_managed_turn_runs_checks_and_seals_ready_proof() {
        use std::{os::unix::fs::PermissionsExt as _, process::Command};

        use crate::{
            managed_provider::{ManagedProviderHandle, ProviderEvent, TurnOutcome},
            mission::{
                claims::ClaimRequestId,
                evidence::{CheckDeclaration, CommandSpec, PathRule},
                model::{MissionDefinition, MissionStatus, ProviderKind, ProviderMode},
                runtime::{ConfigureMission, CreateMission, StartRun},
            },
        };

        fn git(repository: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(repository)
                .status()
                .unwrap();
            assert!(status.success(), "git command failed: {args:?}");
        }

        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "-q"]);
        git(repository.path(), &["config", "user.name", "Nagi Test"]);
        git(
            repository.path(),
            &["config", "user.email", "nagi@example.invalid"],
        );
        let script = repository.path().join("verify");
        std::fs::write(&script, "#!/bin/sh\nprintf 'sealed'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        git(repository.path(), &["add", "verify"]);
        git(repository.path(), &["commit", "-qm", "fixture"]);
        let repository = repository.path().canonicalize().unwrap();
        let mut server = test_headless_server();
        server
            .mission_runtime
            .create_mission(CreateMission {
                mission_id: "mission-proof-worker".into(),
                title: "Seal the worker proof".into(),
                repository_path: repository.to_string_lossy().into_owned(),
                objective: "Run checks away from the server writer".into(),
                acceptance_criteria: vec!["The declared verifier passes".into()],
                at_millis: 1,
            })
            .unwrap();
        let criterion_ids =
            MissionDefinition::criterion_ids(&["The declared verifier passes".to_owned()]);
        server
            .mission_runtime
            .configure_mission(ConfigureMission {
                mission_id: "mission-proof-worker".into(),
                declarations: vec![CheckDeclaration::command(
                    "verify",
                    CommandSpec::new("./verify", [] as [&str; 0], "."),
                    vec![PathRule::All],
                    vec![],
                )
                .covers(criterion_ids)],
                at_millis: 2,
            })
            .unwrap();
        let started = server
            .mission_runtime
            .start_run(StartRun {
                mission_id: "mission-proof-worker".into(),
                run_id: "run-proof-worker".into(),
                provider: ProviderKind::Codex,
                mode: ProviderMode::Managed,
                worktree_path: repository.to_string_lossy().into_owned(),
                request_id: ClaimRequestId::new("run-proof-worker").unwrap(),
                execute_declared_checks: true,
                execute_project_recipe: false,
                at_millis: 3,
            })
            .unwrap();
        server
            .mission_runtime
            .bind_provider_session(
                "mission-proof-worker",
                "run-proof-worker",
                "session-proof-worker",
                4,
            )
            .unwrap();
        let (handle, _commands) = ManagedProviderHandle::for_test(1);
        server.managed_runs.insert(
            "run-proof-worker".into(),
            ManagedRun {
                mission_id: "mission-proof-worker".into(),
                provider: ProviderKind::Codex,
                recovered: false,
                execute_declared_checks: true,
                execute_project_recipe: false,
                handle,
                lease: started.lease,
                responses: HashMap::new(),
                inflight_responses: HashMap::new(),
            },
        );

        assert!(server.handle_provider_event(ProviderEvent::TurnCompleted {
            run_id: "run-proof-worker".into(),
            turn_id: "turn-proof-worker".into(),
            outcome: TurnOutcome::Completed,
        }));
        assert!(server.managed_runs.is_empty());
        assert!(server.pending_proofs.contains_key("run-proof-worker"));
        let event = tokio::time::timeout(Duration::from_secs(10), server.proof_event_rx.recv())
            .await
            .expect("proof worker timed out")
            .expect("proof worker channel closed");
        assert!(server.handle_proof_event(event));

        let mission = server
            .mission_runtime
            .mission("mission-proof-worker")
            .unwrap();
        assert_eq!(mission.status, MissionStatus::ReadyToClose);
        assert!(mission.latest_evidence_pack_digest.is_some());
        assert!(server.pending_proofs.is_empty());

        let close_response = server.handle_mission_close_api(
            "close-proof-worker",
            &crate::api::schema::MissionTarget {
                mission_id: "mission-proof-worker".into(),
            },
        );
        let close_response: crate::api::schema::SuccessResponse =
            serde_json::from_str(&close_response).unwrap();
        assert!(matches!(
            close_response.result,
            crate::api::schema::ResponseResult::MissionCloseAccepted { .. }
        ));
        assert!(server.pending_proofs.contains_key("run-proof-worker"));

        let event = tokio::time::timeout(Duration::from_secs(10), server.proof_event_rx.recv())
            .await
            .expect("archive proof worker timed out")
            .expect("archive proof worker channel closed");
        assert!(server.handle_proof_event(event));
        assert_eq!(
            server
                .mission_runtime
                .mission("mission-proof-worker")
                .unwrap()
                .status,
            MissionStatus::Archived
        );
        assert!(server.pending_proofs.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_recovery_resumes_the_same_session_without_replaying_a_turn() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::mission::{
            claims::ClaimRequestId,
            model::{MissionStatus, ProviderKind, ProviderMode},
            runtime::{CreateMission, MissionRuntime, StartRun},
        };

        let session = tempfile::tempdir().unwrap();
        let claims = session.path().join("claims");
        let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut runtime = MissionRuntime::open_owned(session.path(), &claims).unwrap();
        runtime
            .create_mission(CreateMission {
                mission_id: "mission-server-recovery".into(),
                title: "Recover provider".into(),
                repository_path: repository.to_string_lossy().into_owned(),
                objective: "Resume without duplicate work".into(),
                acceptance_criteria: vec!["No turn is replayed".into()],
                at_millis: 1,
            })
            .unwrap();
        configure_test_mission(&mut runtime, "mission-server-recovery", 1);
        let started = runtime
            .start_run(StartRun {
                mission_id: "mission-server-recovery".into(),
                run_id: "run-server-recovery".into(),
                provider: ProviderKind::Codex,
                mode: ProviderMode::Managed,
                worktree_path: repository.to_string_lossy().into_owned(),
                request_id: ClaimRequestId::new("initial-server-recovery").unwrap(),
                execute_declared_checks: false,
                execute_project_recipe: false,
                at_millis: 2,
            })
            .unwrap();
        runtime
            .bind_provider_session(
                "mission-server-recovery",
                "run-server-recovery",
                "session-server-recovery",
                3,
            )
            .unwrap();
        runtime.release_worktree(&started.lease).unwrap();
        drop(runtime);

        let provider_directory = tempfile::tempdir().unwrap();
        let executable = provider_directory.path().join("recovery-provider");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"session-server-recovery"}}}'
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

        let recovered_runtime = MissionRuntime::open_owned(session.path(), &claims).unwrap();
        let mut server = test_headless_server_with_mission_runtime(recovered_runtime);
        server.managed_provider_executable = Some(executable);
        server.recover_managed_runs();

        assert!(server.managed_runs.contains_key("run-server-recovery"));
        let event = tokio::time::timeout(PROVIDER_FIXTURE_TIMEOUT, server.provider_event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            crate::managed_provider::ProviderEvent::Ready {
                ref run_id,
                ref session_id,
            } if run_id == "run-server-recovery" && session_id == "session-server-recovery"
        ));
        assert!(server.handle_provider_event(event));
        assert_eq!(
            server
                .mission_runtime
                .mission("mission-server-recovery")
                .unwrap()
                .status,
            MissionStatus::Active
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), server.provider_event_rx.recv(),)
                .await
                .is_err(),
            "recovery must not replay the mission prompt as a new turn"
        );
    }

    #[test]
    fn managed_recovery_blocks_ambiguous_preparing_and_attention_states() {
        use crate::mission::{
            attention::AttentionRisk,
            claims::ClaimRequestId,
            model::{MissionStatus, ProviderKind, ProviderMode},
            runtime::{CreateMission, StartRun},
            store::{PersistableMissionEvent, PersistedAttentionState},
        };

        let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut server = test_headless_server();
        for (mission_id, run_id) in [
            ("mission-preparing-recovery", "run-preparing-recovery"),
            ("mission-attention-recovery", "run-attention-recovery"),
        ] {
            server
                .mission_runtime
                .create_mission(CreateMission {
                    mission_id: mission_id.into(),
                    title: "Block ambiguous recovery".into(),
                    repository_path: repository.to_string_lossy().into_owned(),
                    objective: "Never duplicate uncertain provider work".into(),
                    acceptance_criteria: vec!["Recovery is explicit".into()],
                    at_millis: 1,
                })
                .unwrap();
            configure_test_mission(&mut server.mission_runtime, mission_id, 1);
            let started = server
                .mission_runtime
                .start_run(StartRun {
                    mission_id: mission_id.into(),
                    run_id: run_id.into(),
                    provider: ProviderKind::Codex,
                    mode: ProviderMode::Managed,
                    worktree_path: repository.to_string_lossy().into_owned(),
                    request_id: ClaimRequestId::new(run_id).unwrap(),
                    execute_declared_checks: false,
                    execute_project_recipe: false,
                    at_millis: 2,
                })
                .unwrap();
            server
                .mission_runtime
                .release_worktree(&started.lease)
                .unwrap();
            if mission_id == "mission-attention-recovery" {
                server
                    .mission_runtime
                    .bind_provider_session(mission_id, run_id, "session-attention", 3)
                    .unwrap();
                server
                    .mission_runtime
                    .commit(
                        "attention-ambiguous-recovery",
                        PersistableMissionEvent::AttentionChanged {
                            mission_id: mission_id.into(),
                            attention_id: "attention-ambiguous".into(),
                            state: PersistedAttentionState::Open,
                            risk: AttentionRisk::High,
                            at_millis: 4,
                        },
                    )
                    .unwrap();
            }
        }

        server.recover_managed_runs();

        assert!(server.managed_runs.is_empty());
        for mission_id in ["mission-preparing-recovery", "mission-attention-recovery"] {
            assert_eq!(
                server.mission_runtime.mission(mission_id).unwrap().status,
                MissionStatus::Blocked
            );
        }
    }

    /// Verify that no direct calls to `self.app.handle_internal_event`
    /// (or its `handle_internal_event_with_prefix_sync` wrapper) exist
    /// outside of `handle_internal_event_with_forwarding` in this
    /// module. This ensures the forwarding bypass cannot be reintroduced.
    ///
    /// The search pattern looks for `handle_internal_event` calls that
    /// are NOT inside the `handle_internal_event_with_forwarding` method.
    #[test]
    fn no_handle_internal_event_bypass_in_module() {
        let source = include_str!("headless.rs");

        // Find all lines containing handle_internal_event
        let mut bypass_lines: Vec<String> = Vec::new();
        let mut inside_forwarding_method = false;
        let mut forwarding_method_brace_depth = 0u32;

        for (i, line) in source.lines().enumerate() {
            let line_num = i + 1;

            // Track when we're inside handle_internal_event_with_forwarding
            if line.contains("fn handle_internal_event_with_forwarding") {
                inside_forwarding_method = true;
                forwarding_method_brace_depth = 0;
            }

            if inside_forwarding_method {
                // Count braces to track when we exit the method
                for ch in line.chars() {
                    match ch {
                        '{' => forwarding_method_brace_depth += 1,
                        '}' => {
                            forwarding_method_brace_depth =
                                forwarding_method_brace_depth.saturating_sub(1);
                            if forwarding_method_brace_depth == 0 {
                                inside_forwarding_method = false;
                            }
                        }
                        _ => {}
                    }
                }
            } else if (line.contains("self.app.handle_internal_event(")
                || line.contains("self.app.handle_internal_event_with_prefix_sync("))
                && !line.trim().starts_with("///")
                && !line.contains("contains(")
            {
                // Direct call to handle_internal_event outside the forwarding method
                bypass_lines.push(format!("line {}: {}", line_num, line.trim()));
            }
        }

        assert!(
            bypass_lines.is_empty(),
            "Found direct calls to self.app.handle_internal_event outside \
             handle_internal_event_with_forwarding (bypass risk):\n  {}",
            bypass_lines.join("\n  ")
        );
    }
}
